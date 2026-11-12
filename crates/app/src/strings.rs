//! All user-facing strings (English).

pub const APP_TITLE: &str = "Package Manager";
pub const SEARCH_PLACEHOLDER: &str = "Search apps";
pub const UPDATE_ALL: &str = "Update All";
pub const UPDATE: &str = "Update";
pub const CHECK: &str = "Check for updates";
pub const RETRY: &str = "Retry";
pub const LOADING: &str = "Checking for updates…";
pub const UP_TO_DATE: &str = "Everything is up to date.";
pub const NO_MATCHES: &str = "No apps match your search.";
pub const UPDATING: &str = "Updating…";
pub const SHOW_SYSTEM: &str = "Show system packages";
pub const ROW_LIMIT_NOTE: &str = "Use search to narrow the list.";
pub const SHOW_TERMINAL: &str = "Show terminal";
pub const COPY: &str = "Copy";
pub const CLEAR: &str = "Clear";
pub const EXPAND: &str = "Expand";
pub const SHRINK: &str = "Shrink";
pub const JUMP_TO_BOTTOM: &str = "Jump to bottom";
pub const NO_OUTPUT: &str = "No output yet.";
pub const ADD_FROM_GITHUB: &str = "Add from GitHub";
pub const LINK_PLACEHOLDER: &str = "owner/repo or https://github.com/owner/repo/releases";
pub const FIND_RELEASE: &str = "Find release";
pub const CLOSE: &str = "Close";
pub const LINK_SEARCHING: &str = "Looking up the latest release…";
pub const LINK_ONLY: &str = "Link only";
pub const LINK_ONLY_HINT: &str = "Link only keeps it up to date if it is already installed.";
pub const CANCEL: &str = "Cancel";
pub const PACKAGE_NAME: &str = "Package name";
pub const PACKAGE_HINT: &str =
    "Must match the name inside the .deb. The installer refuses the file if it differs.";
pub const NO_CHECKSUM_WARNING: &str = "Warning: No checksum provided by maintainer";
pub const NO_CHECKSUM_CONFIRM: &str = "Install without a checksum (I trust this repository)";
pub const CHECKSUM_OK: &str =
    "The maintainer publishes a checksum; it is verified before installing";
pub const REPLACE_WARNING: &str =
    "Warning: this package is already installed (possibly from your distribution). Installing replaces it.";
pub const REPLACE_CONFIRM: &str = "Replace the installed package with this download";
