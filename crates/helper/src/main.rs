//! Privileged helper, invoked as `pkexec package-manager-helper <command> [args]`.
//! Output is inherited, so the GUI sees every line as the child writes it (no buffering here).

use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

use package_manager_helper::install::{
    copy_and_verify, home_for_uid, normalize_child_code, open_validated, private_run_dir,
    refusal_message, sanitize_for_log, sweep_stale_run_dirs, DOWNLOADS_SUBDIR,
};
use package_manager_helper::{parse, Command as Cmd, InstallRequest};

const LOG_PATH: &str = "/var/log/package-manager-helper.log";
/// On disk (not `/run`, which is RAM-backed) because packages can be large. Under `/var/lib`,
/// which only root can write to, so nobody else can pre-create or swap this folder.
const RUN_BASE: &str = "/var/lib/package-manager-helper";
const SAFE_PATH: &str = "/usr/sbin:/usr/bin:/sbin:/bin";
const DPKG: &str = "/usr/bin/dpkg";
const DPKG_DEB: &str = "/usr/bin/dpkg-deb";
const APT_GET: &str = "/usr/bin/apt-get";

/// Appends to a root-owned log. Best effort: logging must never block the update.
fn log(line: &str) {
    let uid = std::env::var("PKEXEC_UID").unwrap_or_else(|_| "?".to_owned());
    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
        .and_then(|mut f| {
            writeln!(
                f,
                "uid={} {}",
                sanitize_for_log(&uid),
                sanitize_for_log(line)
            )
        });
    if let Err(e) = result {
        eprintln!("package-manager-helper: could not write {LOG_PATH}: {e}");
    }
}

fn say(line: &str) {
    println!("{line}");
    let _ = std::io::stdout().flush();
}

/// A root process that never inherits the caller's environment.
fn root_command(program: &str) -> Command {
    let mut c = Command::new(program);
    c.env_clear()
        .env("PATH", SAFE_PATH)
        .env("LC_ALL", "C")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .stdin(Stdio::null());
    c
}

fn exit_code(status: ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|n| 128 + n))
        .unwrap_or(1)
}

fn run_logged(program: &str, args: &[&str]) -> i32 {
    say(&format!("$ {program} {}", args.join(" ")));
    match root_command(program).args(args).status() {
        Ok(s) => normalize_child_code(exit_code(s)),
        Err(e) => {
            eprintln!("package-manager-helper: could not start {program}: {e}");
            3
        }
    }
}

/// The user who ran pkexec, from the variable pkexec sets. Never taken from the arguments.
fn caller_uid() -> Option<u32> {
    std::env::var("PKEXEC_UID").ok()?.parse().ok()
}

fn fail(message: &str, code: i32) -> i32 {
    let message = sanitize_for_log(message);
    eprintln!("package-manager-helper: {message}");
    log(&format!("failed: {message}"));
    code
}

/// Copy, verify and install a downloaded `.deb`. Returns the exit code.
fn install_deb(req: &InstallRequest) -> i32 {
    let Some(uid) = caller_uid() else {
        return fail("install-deb must be started through pkexec", 2);
    };
    let passwd = std::fs::read_to_string("/etc/passwd").unwrap_or_default();
    let Some(home) = home_for_uid(&passwd, uid) else {
        return fail("could not find the requesting user's home folder", 2);
    };
    let allowed: PathBuf = home.join(DOWNLOADS_SUBDIR);

    let source = match open_validated(&req.path, &allowed, uid) {
        Ok(f) => f,
        Err(e) => return fail(&refusal_message(&req.path, &e), 4),
    };
    sweep_stale_run_dirs(Path::new(RUN_BASE));
    let run_dir = match private_run_dir(Path::new(RUN_BASE)) {
        Ok(d) => d,
        Err(e) => return fail(&format!("could not prepare a private folder: {e}"), 3),
    };
    let copy = run_dir.join("package.deb");
    let code = install_verified_copy(source, &copy, req);
    let _ = std::fs::remove_dir_all(&run_dir);
    code
}

fn install_verified_copy(source: std::fs::File, copy: &Path, req: &InstallRequest) -> i32 {
    // Hash and copy in one pass: what is installed is exactly what was verified.
    if let Err(e) = copy_and_verify(source, copy, &req.hash) {
        return fail(&format!("the download was rejected: {e}"), 5);
    }
    say("Checksum verified");

    // The file must be the package the plan approved, not some other package.
    let Some(copy_str) = copy.to_str() else {
        return fail("invalid path", 3);
    };
    let declared = match root_command(DPKG_DEB)
        .args(["-f", copy_str, "Package"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_owned(),
        _ => return fail("could not read the package name from the .deb", 5),
    };
    if declared != req.package {
        return fail(
            &format!(
                "the .deb is package {declared:?}, but {:?} was approved",
                req.package
            ),
            5,
        );
    }
    log(&format!("installing {declared} sha-verified"));

    let code = run_logged(
        DPKG,
        &["--force-confold", "--force-confdef", "-i", copy_str],
    );
    if code == 0 {
        return 0;
    }
    // Missing dependencies are the usual reason: let APT fetch them and finish configuring.
    say("dpkg reported a problem; letting APT fix dependencies");
    run_logged(APT_GET, &["install", "-f", "-y"])
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("package-manager-helper: {e}");
            log(&format!("rejected: {e}"));
            std::process::exit(2);
        }
    };

    let code = match &command {
        Cmd::InstallDeb(req) => {
            log(&format!("run install-deb: package {}", req.package));
            install_deb(req)
        }
        other => match other.argv() {
            Some((program, argv)) => {
                log(&format!(
                    "run {}: {program} {}",
                    other.name(),
                    argv.join(" ")
                ));
                let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                run_logged(program, &refs)
            }
            None => fail("command has no program to run", 2),
        },
    };
    log(&format!(
        "finished {} with exit code {code}",
        command.name()
    ));
    std::process::exit(code);
}
