//! `fbk cert trust` and `fbk cert clean`: add the daemon's localhost certificate authority to
//! the host's trust stores, or remove it from them (ADR 0065). Forwarded ports serve HTTPS with
//! certificates it signs, so a browser that trusts it opens the HTTPS dev servers of sandboxes
//! without a warning.
//!
//! [`plan`] decides which commands change which store, from what [`Host::detect`] found on the
//! host, and [`execute`] runs them. On Linux that's the system store and the NSS databases of
//! Chrome and Firefox, on macOS the login keychain and on Windows the user's root store.

use anyhow::{Context, Result, bail};
use firebrick_utils::LOCALHOST_CA_NAME;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The name of the certificate authority's file in the Linux system store.
const SYSTEM_FILE_NAME: &str = "firebrick-localhost-ca.crt";

/// How often [`Step::RemoveAll`] removes a certificate before it gives up, in case the removal
/// succeeds without removing anything.
const MAX_REMOVALS: usize = 16;

/// What to do with the certificate authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Trust the certificate authority in the PEM file, replacing earlier ones.
    Trust(PathBuf),
    /// Remove every firebrick certificate authority.
    Clean,
}

/// The operating systems with trust stores `fbk` knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// Linux, with a system store and NSS databases.
    Linux,
    /// macOS, with the login keychain.
    MacOs,
    /// Windows, with the user's root store.
    Windows,
}

/// A Linux system trust store: a directory of anchors and the command that rebuilds the bundles
/// from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemStore {
    /// The directory of the anchors.
    pub dir: PathBuf,
    /// The command, with its arguments, that rebuilds the bundles.
    pub update: Vec<String>,
    /// Whether the certificate authority's file is in the directory.
    pub has_ca: bool,
}

/// What [`plan`] needs to know about the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    /// The operating system.
    pub platform: Platform,
    /// The user's home directory.
    pub home: PathBuf,
    /// The Linux system store, when one was found.
    pub system: Option<SystemStore>,
    /// The NSS databases of Chrome and Firefox on Linux.
    pub nss_dbs: Vec<PathBuf>,
    /// Whether NSS's `certutil` is installed on Linux.
    pub has_nss_certutil: bool,
}

/// The Linux system stores `fbk` knows, as anchor directories and the commands that rebuild
/// their bundles: Fedora and RHEL, Debian and Ubuntu, Arch, and openSUSE.
const SYSTEM_STORES: [(&str, &[&str]); 4] = [
    (
        "/etc/pki/ca-trust/source/anchors",
        &["update-ca-trust", "extract"],
    ),
    (
        "/usr/local/share/ca-certificates",
        &["update-ca-certificates"],
    ),
    (
        "/etc/ca-certificates/trust-source/anchors",
        &["trust", "extract-compat"],
    ),
    ("/usr/share/pki/trust/anchors", &["update-ca-certificates"]),
];

/// The directories under the home directory that hold Firefox profiles: the package, the Snap
/// and the Flatpak.
const FIREFOX_DIRS: [&str; 3] = [
    ".mozilla/firefox",
    "snap/firefox/common/.mozilla/firefox",
    ".var/app/org.mozilla.firefox/.mozilla/firefox",
];

/// The NSS databases of Chrome and Chromium under the home directory.
const CHROME_DBS: [&str; 2] = [".pki/nssdb", "snap/chromium/current/.pki/nssdb"];

impl Host {
    /// Looks up the platform, the home directory and the trust stores of this host.
    pub fn detect() -> Result<Self> {
        let home = std::env::var_os(firebrick_utils::HOME_VAR)
            .map(PathBuf::from)
            .with_context(|| format!("${} isn't set", firebrick_utils::HOME_VAR))?;
        let platform = match std::env::consts::OS {
            "linux" => Platform::Linux,
            "macos" => Platform::MacOs,
            "windows" => Platform::Windows,
            other => bail!("fbk can't manage the trust stores of {other}"),
        };
        let linux = platform == Platform::Linux;

        Ok(Self {
            platform,
            system: linux.then(find_system_store).flatten(),
            nss_dbs: if linux { find_nss_dbs(&home) } else { vec![] },
            has_nss_certutil: linux && on_path("certutil"),
            home,
        })
    }
}

/// Returns the first system store whose directory exists.
fn find_system_store() -> Option<SystemStore> {
    SYSTEM_STORES
        .iter()
        .map(|(dir, update)| (Path::new(dir), update))
        .find(|(dir, _)| dir.is_dir())
        .map(|(dir, update)| SystemStore {
            dir: dir.to_path_buf(),
            update: update.iter().map(ToString::to_string).collect(),
            has_ca: dir.join(SYSTEM_FILE_NAME).exists(),
        })
}

/// Returns the NSS databases of Chrome and of the Firefox profiles under `home`.
fn find_nss_dbs(home: &Path) -> Vec<PathBuf> {
    let chrome = CHROME_DBS.iter().map(|dir| home.join(dir));
    let firefox = FIREFOX_DIRS
        .iter()
        .filter_map(|dir| std::fs::read_dir(home.join(dir)).ok())
        .flat_map(|entries| entries.filter_map(|entry| entry.ok().map(|entry| entry.path())));

    chrome
        .chain(firefox)
        .filter(|dir| dir.join("cert9.db").is_file())
        .collect()
}

/// Whether an executable file with the name is in a directory on `PATH`.
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(name).is_file()))
}

/// A command `fbk` runs to change a trust store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// The program.
    pub program: String,
    /// Its arguments.
    pub args: Vec<OsString>,
}

impl Command {
    /// Returns the command that runs the program with the arguments.
    fn new<I, S>(program: &str, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self {
            program: program.to_string(),
            args: args
                .into_iter()
                .map(|arg| arg.as_ref().to_owned())
                .collect(),
        }
    }

    /// Returns the command line as it would be typed, for messages.
    fn display(&self) -> String {
        std::iter::once(self.program.clone())
            .chain(
                self.args
                    .iter()
                    .map(|arg| arg.to_string_lossy().into_owned()),
            )
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// One change to a trust store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Runs the command, which has to succeed.
    Run(Command),
    /// Runs `remove` as long as `present` succeeds, to remove every copy of a certificate.
    RemoveAll {
        /// Succeeds while the store holds the certificate.
        present: Command,
        /// Removes one copy of the certificate.
        remove: Command,
    },
}

/// The steps that change one trust store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePlan {
    /// The store, for messages, such as `the NSS database ~/.pki/nssdb`.
    pub name: String,
    /// The steps, in order.
    pub steps: Vec<Step>,
}

/// What [`execute`] does: the stores it changes and the notes it prints afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The stores, in order.
    pub stores: Vec<StorePlan>,
    /// Notes about stores it can't change.
    pub notes: Vec<String>,
}

/// Returns the plan that carries out the action on the host's trust stores.
pub fn plan(host: &Host, action: &Action) -> Plan {
    match host.platform {
        Platform::Linux => linux_plan(host, action),
        Platform::MacOs => single_store(macos_store(&host.home, action)),
        Platform::Windows => single_store(windows_store(action)),
    }
}

/// Returns the plan that changes only the store.
fn single_store(store: StorePlan) -> Plan {
    Plan {
        stores: vec![store],
        notes: vec![],
    }
}

/// Returns the plan for the Linux system store and the NSS databases.
fn linux_plan(host: &Host, action: &Action) -> Plan {
    let mut stores = vec![];
    let mut notes = vec![];

    match (&host.system, action) {
        (Some(system), Action::Trust(_)) | (Some(system @ SystemStore { has_ca: true, .. }), _) => {
            stores.push(system_store(system, action));
        }
        (Some(_), Action::Clean) => {}
        (None, Action::Trust(ca)) => notes.push(format!(
            "fbk couldn't find the system trust store; add {} to it by hand",
            ca.display()
        )),
        (None, Action::Clean) => {}
    }

    if host.has_nss_certutil {
        stores.extend(host.nss_dbs.iter().map(|db| nss_store(db, action)));
    } else if !host.nss_dbs.is_empty() {
        notes.push(
            "install certutil (libnss3-tools on Debian and Ubuntu, nss-tools on Fedora, nss on \
             Arch) and run this again to update Chrome and Firefox"
                .to_string(),
        );
    }

    Plan { stores, notes }
}

/// Returns the steps that add the certificate authority to the system store, or remove it.
fn system_store(system: &SystemStore, action: &Action) -> StorePlan {
    let target = system.dir.join(SYSTEM_FILE_NAME);
    let change = match action {
        Action::Trust(ca) => Command::new(
            "sudo",
            [
                OsStr::new("install"),
                OsStr::new("-m"),
                OsStr::new("644"),
                ca.as_os_str(),
                target.as_os_str(),
            ],
        ),
        Action::Clean => Command::new(
            "sudo",
            [OsStr::new("rm"), OsStr::new("-f"), target.as_os_str()],
        ),
    };
    let update = Command::new("sudo", &system.update);

    StorePlan {
        name: format!("the system trust store ({})", system.dir.display()),
        steps: vec![Step::Run(change), Step::Run(update)],
    }
}

/// Returns the steps that replace the certificate authority in the NSS database, or remove it.
fn nss_store(db: &Path, action: &Action) -> StorePlan {
    let mut database = OsString::from("sql:");
    database.push(db);
    let named = |flag: &str| {
        Command::new(
            "certutil",
            [
                OsStr::new(flag),
                OsStr::new("-d"),
                &database,
                OsStr::new("-n"),
                OsStr::new(LOCALHOST_CA_NAME),
            ],
        )
    };
    let mut steps = vec![Step::RemoveAll {
        present: named("-L"),
        remove: named("-D"),
    }];

    if let Action::Trust(ca) = action {
        let mut add = named("-A");
        add.args.extend(["-t", "C,,", "-i"].map(OsString::from));
        add.args.push(ca.clone().into_os_string());
        steps.push(Step::Run(add));
    }

    StorePlan {
        name: format!("the NSS database {}", db.display()),
        steps,
    }
}

/// Returns the steps that replace the certificate authority in the login keychain, or remove
/// it.
fn macos_store(home: &Path, action: &Action) -> StorePlan {
    let keychain = home.join("Library/Keychains/login.keychain-db");
    let named = |args: &[&str]| {
        let mut command = Command::new("security", args);
        command.args.extend([
            OsString::from("-c"),
            OsString::from(LOCALHOST_CA_NAME),
            keychain.clone().into_os_string(),
        ]);
        command
    };
    let mut steps = vec![Step::RemoveAll {
        present: named(&["find-certificate"]),
        remove: named(&["delete-certificate", "-t"]),
    }];

    if let Action::Trust(ca) = action {
        steps.push(Step::Run(Command::new(
            "security",
            [
                OsStr::new("add-trusted-cert"),
                OsStr::new("-r"),
                OsStr::new("trustRoot"),
                OsStr::new("-k"),
                keychain.as_os_str(),
                ca.as_os_str(),
            ],
        )));
    }

    StorePlan {
        name: "the login keychain".to_string(),
        steps,
    }
}

/// Returns the steps that replace the certificate authority in the user's root store, or remove
/// it.
fn windows_store(action: &Action) -> StorePlan {
    let named = |verb: &str| Command::new("certutil", ["-user", verb, "Root", LOCALHOST_CA_NAME]);
    let mut steps = vec![Step::RemoveAll {
        present: named("-store"),
        remove: named("-delstore"),
    }];

    if let Action::Trust(ca) = action {
        steps.push(Step::Run(Command::new(
            "certutil",
            [
                OsStr::new("-user"),
                OsStr::new("-addstore"),
                OsStr::new("Root"),
                ca.as_os_str(),
            ],
        )));
    }

    StorePlan {
        name: "the Windows root store of the user".to_string(),
        steps,
    }
}

/// Runs the plan's steps store by store, printing each store it changed, then prints the
/// notes. Stops at the first step that fails, keeping the stores changed before it.
pub fn execute(plan: &Plan, action: &Action) -> Result<()> {
    for store in &plan.stores {
        for step in &store.steps {
            run_step(step).with_context(|| match action {
                Action::Trust(_) => format!("couldn't trust the certificate in {}", store.name),
                Action::Clean => format!("couldn't remove the certificate from {}", store.name),
            })?;
        }

        match action {
            Action::Trust(_) => println!("Trusted the firebrick localhost CA in {}.", store.name),
            Action::Clean => println!("Removed the firebrick localhost CA from {}.", store.name),
        }
    }

    for note in &plan.notes {
        eprintln!("note: {note}");
    }

    if matches!(action, Action::Trust(_)) && !plan.stores.is_empty() {
        println!("Restart your browser to pick it up.");
    } else if plan.stores.is_empty() && plan.notes.is_empty() {
        println!("No trust store holds the firebrick localhost CA.");
    }

    Ok(())
}

/// Runs one step.
fn run_step(step: &Step) -> Result<()> {
    match step {
        Step::Run(command) => run(command),
        Step::RemoveAll { present, remove } => remove_all(present, remove),
    }
}

/// Runs `remove` as long as `present` succeeds, at most [`MAX_REMOVALS`] times.
fn remove_all(present: &Command, remove: &Command) -> Result<()> {
    for _ in 0..MAX_REMOVALS {
        if !output(present)?.status.success() {
            return Ok(());
        }

        run(remove)?;
    }

    bail!("{} didn't remove the certificate", remove.display())
}

/// Runs the command and fails with its error output when it fails.
fn run(command: &Command) -> Result<()> {
    let output = output(command)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("{} failed: {}", command.display(), stderr.trim());
    }

    Ok(())
}

/// Runs the command with the terminal as its input, so `sudo` can ask for a password, and
/// returns its output.
fn output(command: &Command) -> Result<std::process::Output> {
    std::process::Command::new(&command.program)
        .args(&command.args)
        .stdin(Stdio::inherit())
        .output()
        .with_context(|| format!("couldn't run {}", command.program))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linux_host(system: Option<SystemStore>, nss_dbs: &[&str], certutil: bool) -> Host {
        Host {
            platform: Platform::Linux,
            home: PathBuf::from("/home/me"),
            system,
            nss_dbs: nss_dbs.iter().map(PathBuf::from).collect(),
            has_nss_certutil: certutil,
        }
    }

    fn debian(has_ca: bool) -> SystemStore {
        SystemStore {
            dir: PathBuf::from("/usr/local/share/ca-certificates"),
            update: vec!["update-ca-certificates".to_string()],
            has_ca,
        }
    }

    fn trust() -> Action {
        Action::Trust(PathBuf::from("/data/tls/ca.crt"))
    }

    /// Returns the command lines of the steps, with `present => remove` for removals.
    fn lines(store: &StorePlan) -> Vec<String> {
        store
            .steps
            .iter()
            .map(|step| match step {
                Step::Run(command) => command.display(),
                Step::RemoveAll { present, remove } => {
                    format!("{} => {}", present.display(), remove.display())
                }
            })
            .collect()
    }

    /// Returns where the system store keeps the certificate authority, as the plan writes it.
    fn system_file() -> String {
        debian(false)
            .dir
            .join(SYSTEM_FILE_NAME)
            .display()
            .to_string()
    }

    #[test]
    fn linux_trust_installs_into_the_system_store_and_replaces_in_nss_databases() {
        let host = linux_host(Some(debian(false)), &["/home/me/.pki/nssdb"], true);

        let plan = plan(&host, &trust());

        assert!(plan.notes.is_empty());
        assert_eq!(plan.stores.len(), 2);
        assert_eq!(
            lines(&plan.stores[0]),
            [
                format!("sudo install -m 644 /data/tls/ca.crt {}", system_file()),
                "sudo update-ca-certificates".to_string(),
            ]
        );
        assert_eq!(
            lines(&plan.stores[1]),
            [
                "certutil -L -d sql:/home/me/.pki/nssdb -n Firebrick localhost CA => \
                 certutil -D -d sql:/home/me/.pki/nssdb -n Firebrick localhost CA",
                "certutil -A -d sql:/home/me/.pki/nssdb -n Firebrick localhost CA -t C,, \
                 -i /data/tls/ca.crt",
            ]
        );
    }

    #[test]
    fn linux_clean_removes_the_system_file_and_every_nss_copy() {
        let host = linux_host(Some(debian(true)), &["/home/me/.pki/nssdb"], true);

        let plan = plan(&host, &Action::Clean);

        assert_eq!(
            lines(&plan.stores[0]),
            [
                format!("sudo rm -f {}", system_file()),
                "sudo update-ca-certificates".to_string(),
            ]
        );
        assert_eq!(
            lines(&plan.stores[1]),
            [
                "certutil -L -d sql:/home/me/.pki/nssdb -n Firebrick localhost CA => \
              certutil -D -d sql:/home/me/.pki/nssdb -n Firebrick localhost CA"
            ]
        );
    }

    #[test]
    fn linux_clean_skips_a_system_store_without_the_ca() {
        let host = linux_host(Some(debian(false)), &[], true);

        let plan = plan(&host, &Action::Clean);

        assert!(plan.stores.is_empty());
        assert!(plan.notes.is_empty());
    }

    #[test]
    fn linux_without_a_system_store_or_certutil_explains_what_to_do() {
        let host = linux_host(None, &["/home/me/.pki/nssdb"], false);

        let plan = plan(&host, &trust());

        assert!(plan.stores.is_empty());
        assert_eq!(plan.notes.len(), 2);
        assert!(plan.notes[0].contains("/data/tls/ca.crt"));
        assert!(plan.notes[1].contains("install certutil"));
    }

    #[test]
    fn macos_trust_replaces_the_ca_in_the_login_keychain() {
        let host = Host {
            platform: Platform::MacOs,
            ..linux_host(None, &[], false)
        };
        let keychain = host.home.join("Library/Keychains/login.keychain-db");
        let keychain = keychain.display();

        let plan = plan(&host, &trust());

        assert_eq!(
            lines(&plan.stores[0]),
            [
                format!(
                    "security find-certificate -c Firebrick localhost CA {keychain} => \
                     security delete-certificate -t -c Firebrick localhost CA {keychain}"
                ),
                format!("security add-trusted-cert -r trustRoot -k {keychain} /data/tls/ca.crt"),
            ]
        );
    }

    #[test]
    fn windows_clean_removes_the_ca_from_the_user_root_store() {
        let host = Host {
            platform: Platform::Windows,
            ..linux_host(None, &[], false)
        };

        let plan = plan(&host, &Action::Clean);

        assert_eq!(
            lines(&plan.stores[0]),
            ["certutil -user -store Root Firebrick localhost CA => \
              certutil -user -delstore Root Firebrick localhost CA"]
        );
    }
}
