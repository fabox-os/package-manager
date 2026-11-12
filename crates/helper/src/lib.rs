//! Argument parsing and validation for the privileged helper.
//!
//! This is the security boundary: the helper runs as root through `pkexec`, so it accepts
//! only a closed set of commands and rejects every argument it cannot prove is safe.
//! It never invokes a shell and always uses absolute program paths.

pub mod install;

use std::fmt;
use std::path::PathBuf;

use install::HashSpec;

pub const APT_GET: &str = "/usr/bin/apt-get";
pub const MAX_PACKAGES: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `apt-get update`
    AptUpdate,
    /// Upgrade only the listed, already-installed packages.
    AptUpgrade(Vec<String>),
    /// `apt-get install -f`: repair broken dependencies.
    AptFix,
    /// Install a downloaded `.deb` that the app verified. See [`install`].
    InstallDeb(InstallRequest),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRequest {
    pub path: PathBuf,
    /// The package the plan approved; the `.deb` must declare exactly this name.
    pub package: String,
    /// Checksum the installed bytes must match.
    pub hash: HashSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageError {
    MissingCommand,
    UnknownCommand(String),
    UnexpectedArguments(&'static str),
    NoPackages,
    TooManyPackages,
    InvalidPackageName(String),
    InvalidInstallArguments,
}

impl fmt::Display for UsageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommand => {
                write!(
                    f,
                    "missing command (apt-update | apt-upgrade | apt-fix | install-deb)"
                )
            }
            Self::UnknownCommand(c) => write!(f, "unknown command: {c:?}"),
            Self::UnexpectedArguments(c) => write!(f, "{c} takes no arguments"),
            Self::NoPackages => write!(f, "apt-upgrade needs at least one package"),
            Self::TooManyPackages => write!(f, "too many packages (max {MAX_PACKAGES})"),
            Self::InvalidPackageName(p) => write!(f, "invalid package name: {p:?}"),
            Self::InvalidInstallArguments => write!(
                f,
                "usage: install-deb <absolute .deb path> <package> <sha256|sha512> <hex checksum>"
            ),
        }
    }
}

/// Debian policy: at least two characters, lowercase letters, digits and `+ - .`,
/// starting with a letter or digit. A leading `-` (option injection) can never match.
pub fn is_valid_package_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    first_ok
        && name.len() >= 2
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '+' | '-' | '.'))
}

pub fn parse(args: &[String]) -> Result<Command, UsageError> {
    let (cmd, rest) = args.split_first().ok_or(UsageError::MissingCommand)?;
    match cmd.as_str() {
        "apt-update" if rest.is_empty() => Ok(Command::AptUpdate),
        "apt-update" => Err(UsageError::UnexpectedArguments("apt-update")),
        "apt-fix" if rest.is_empty() => Ok(Command::AptFix),
        "apt-fix" => Err(UsageError::UnexpectedArguments("apt-fix")),
        "apt-upgrade" => {
            if rest.is_empty() {
                return Err(UsageError::NoPackages);
            }
            if rest.len() > MAX_PACKAGES {
                return Err(UsageError::TooManyPackages);
            }
            if let Some(bad) = rest.iter().find(|p| !is_valid_package_name(p)) {
                return Err(UsageError::InvalidPackageName(bad.clone()));
            }
            Ok(Command::AptUpgrade(rest.to_vec()))
        }
        "install-deb" => parse_install(rest),
        other => Err(UsageError::UnknownCommand(other.to_owned())),
    }
}

fn parse_install(rest: &[String]) -> Result<Command, UsageError> {
    let [path, package, algo, hex] = rest else {
        return Err(UsageError::InvalidInstallArguments);
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() || path.as_os_str().as_encoded_bytes().contains(&0) {
        return Err(UsageError::InvalidInstallArguments);
    }
    if !is_valid_package_name(package) {
        return Err(UsageError::InvalidPackageName(package.clone()));
    }
    let hash = HashSpec::parse(algo, &hex.to_ascii_lowercase())
        .ok_or(UsageError::InvalidInstallArguments)?;
    Ok(Command::InstallDeb(InstallRequest {
        path,
        package: package.clone(),
        hash,
    }))
}

impl Command {
    /// Program and arguments for the single-process commands. `None` for `install-deb`,
    /// which is a multi-step flow (see [`install`]).
    pub fn argv(&self) -> Option<(&'static str, Vec<String>)> {
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
        match self {
            Self::AptUpdate => Some((APT_GET, s(&["update"]))),
            Self::AptFix => Some((APT_GET, s(&["install", "-f", "-y"]))),
            Self::InstallDeb(_) => None,
            Self::AptUpgrade(pkgs) => {
                // `--only-upgrade` never installs a package that is not already installed.
                // `--` ends option parsing as a second guard against option injection.
                let mut a = s(&[
                    "install",
                    "--only-upgrade",
                    "-y",
                    "-o",
                    "Dpkg::Options::=--force-confold",
                    "-o",
                    "Dpkg::Options::=--force-confdef",
                    "--",
                ]);
                a.extend(pkgs.iter().cloned());
                Some((APT_GET, a))
            }
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::AptUpdate => "apt-update",
            Self::AptFix => "apt-fix",
            Self::AptUpgrade(_) => "apt-upgrade",
            Self::InstallDeb(_) => "install-deb",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn accepts_real_package_names() {
        for ok in [
            "firefox",
            "libstdc++6",
            "python3.12",
            "g++-13",
            "0ad",
            "libgphoto2-6t64",
        ] {
            assert!(is_valid_package_name(ok), "{ok}");
        }
    }

    #[test]
    fn rejects_option_injection_and_junk() {
        for bad in [
            "",
            "a",
            "-o",
            "--help",
            "-oAPT::Update::Pre-Invoke::=sh",
            "Firefox",
            "a b",
            "a;b",
            "a$(id)",
            "../x",
            "/etc/passwd",
            "pkg=1.0",
            "pkg/noble",
            "a\n",
            "é1",
        ] {
            assert!(!is_valid_package_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn parses_known_commands() {
        assert_eq!(parse(&v(&["apt-update"])), Ok(Command::AptUpdate));
        assert_eq!(parse(&v(&["apt-fix"])), Ok(Command::AptFix));
        assert_eq!(
            parse(&v(&["apt-upgrade", "vlc", "firefox"])),
            Ok(Command::AptUpgrade(v(&["vlc", "firefox"])))
        );
    }

    #[test]
    fn rejects_bad_usage() {
        assert_eq!(parse(&[]), Err(UsageError::MissingCommand));
        assert_eq!(
            parse(&v(&["rm"])),
            Err(UsageError::UnknownCommand("rm".into()))
        );
        assert_eq!(
            parse(&v(&["apt-update", "x"])),
            Err(UsageError::UnexpectedArguments("apt-update"))
        );
        assert_eq!(parse(&v(&["apt-upgrade"])), Err(UsageError::NoPackages));
        assert!(matches!(
            parse(&v(&["apt-upgrade", "ok", "--help"])),
            Err(UsageError::InvalidPackageName(_))
        ));
        let many: Vec<String> = std::iter::once("apt-upgrade".to_owned())
            .chain((0..=MAX_PACKAGES).map(|i| format!("pkg{i}")))
            .collect();
        assert_eq!(parse(&many), Err(UsageError::TooManyPackages));
    }

    #[test]
    fn upgrade_argv_has_separator_before_packages_and_absolute_program() {
        let (prog, args) = Command::AptUpgrade(v(&["vlc"]))
            .argv()
            .expect("single process");
        assert!(prog.starts_with('/'));
        let sep = args.iter().position(|a| a == "--").expect("separator");
        assert_eq!(args[sep + 1..], ["vlc"]);
        assert!(args.contains(&"--only-upgrade".to_owned()));
    }

    fn install_args(path: &str, pkg: &str, algo: &str, hex: &str) -> Vec<String> {
        v(&["install-deb", path, pkg, algo, hex])
    }

    #[test]
    fn install_deb_parses_a_complete_request() {
        let hex = "ab".repeat(32);
        let c = parse(&install_args(
            "/home/u/.cache/x/a.deb",
            "hydralauncher",
            "sha256",
            &hex,
        ))
        .expect("ok");
        let Command::InstallDeb(r) = c else {
            panic!("wrong command")
        };
        assert_eq!(r.package, "hydralauncher");
        assert_eq!(r.path, PathBuf::from("/home/u/.cache/x/a.deb"));
        assert!(matches!(r.hash, HashSpec::Sha256(_)));
        // Upper-case hex is accepted and normalized.
        assert!(parse(&install_args("/a.deb", "pkg1", "sha256", &"AB".repeat(32))).is_ok());
        assert!(parse(&install_args("/a.deb", "pkg1", "sha512", &"ab".repeat(64))).is_ok());
        assert!(Command::InstallDeb(r).argv().is_none());
    }

    #[test]
    fn install_deb_rejects_anything_unexpected() {
        let hex = "ab".repeat(32);
        for args in [
            v(&["install-deb"]),
            v(&["install-deb", "/a.deb", "pkg1", "sha256"]),
            v(&["install-deb", "/a.deb", "pkg1", "sha256", &hex, "extra"]),
            install_args("relative/a.deb", "pkg1", "sha256", &hex),
            install_args("-/a.deb", "pkg1", "sha256", &hex),
            install_args("/a.deb", "--help", "sha256", &hex),
            install_args("/a.deb", "Pkg", "sha256", &hex),
            install_args("/a.deb", "pkg1", "md5", &hex),
            install_args("/a.deb", "pkg1", "sha256", &"ab".repeat(31)),
            install_args("/a.deb", "pkg1", "sha512", &hex),
            install_args("/a.deb", "pkg1", "sha256", "not hex"),
            install_args("/a\0.deb", "pkg1", "sha256", &hex),
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }
}
