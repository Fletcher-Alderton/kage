//! Update discovery for the engine and this client.
//!
//! GitHub's releases list is the one source the check reads: a
//! `v<semver>` release carries the engine, this client with that
//! engine beside it, and the web client, all at one version. The
//! rolling nightly is the `nightly` tag, whose commit comes from the
//! tag ref. The parse half is pure,
//! so tests run without a network; the fetch and install half exists
//! only on native targets, and every entry point degrades to a no-op
//! or a report on the web build.

/// The releases list the check reads, newest first. `per_page` reaches
/// past the nightly and any `kage-desktop-v*` release left from when
/// the client shipped on its own, which the parse skips.
#[cfg(not(target_arch = "wasm32"))]
const RELEASES_API: &str = "https://api.github.com/repos/QaidVoid/kage/releases?per_page=50";

/// The tag ref the nightly channel reads for the built commit.
#[cfg(not(target_arch = "wasm32"))]
const NIGHTLY_REF_API: &str = "https://api.github.com/repos/QaidVoid/kage/git/ref/tags/nightly";

/// The page a download action opens in the user's browser, for the
/// channel's releases.
#[must_use]
pub fn releases_page(channel: crate::prefs::Channel) -> &'static str {
    match channel {
        crate::prefs::Channel::Latest => "https://github.com/QaidVoid/kage/releases/latest",
        crate::prefs::Channel::Nightly => "https://github.com/QaidVoid/kage/releases/tags/nightly",
    }
}

/// Where release artifacts live.
const RELEASES_DOWNLOAD: &str = "https://github.com/QaidVoid/kage/releases/download";

/// How long a check result stands before the next launch checks again.
#[cfg(not(target_arch = "wasm32"))]
const CHECK_INTERVAL: i64 = 24 * 60 * 60;

/// The most bytes a downloaded engine archive may weigh. The archive
/// has stayed far under this; the cap bounds a broken mirror's reply.
#[cfg(not(target_arch = "wasm32"))]
const BINARY_CAP: usize = 256 * 1024 * 1024;

/// One release the feed named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The full tag, such as `v0.2.0`.
    pub tag: String,
    /// The version without the engine's leading `v`.
    pub version: String,
}

/// The rolling nightly the feed names. Its commit comes from the
/// tag ref, which the fetch half reads separately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NightlyRelease {
    /// The UTC date it was published, `YYYY-MM-DD`.
    pub date: String,
}

/// The newest releases the feed names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Feed {
    /// The newest release, the first parsable `v*` tag.
    pub latest: Option<Release>,
    /// The `nightly` rolling release, when it is on the list.
    pub nightly: Option<NightlyRelease>,
}

/// Reads the releases list. The first parsable `v*` tag wins, which is
/// GitHub's newest; prerelease suffixes do not parse and are skipped
/// so a broken comparison never reaches the interface.
#[must_use]
pub fn parse_feed(text: &str) -> Feed {
    let mut feed = Feed::default();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return feed;
    };
    let Some(releases) = value.as_array() else {
        return feed;
    };
    for release in releases {
        let Some(tag) = release.get("tag_name").and_then(|tag| tag.as_str()) else {
            continue;
        };
        if tag == "nightly" && feed.nightly.is_none() {
            feed.nightly = release
                .get("published_at")
                .and_then(|at| at.as_str())
                .filter(|at| at.len() >= 10)
                .map(|at| NightlyRelease {
                    date: at[..10].to_owned(),
                });
        } else if let Some(version) = tag.strip_prefix('v')
            && feed.latest.is_none()
            && parses(version)
        {
            feed.latest = Some(Release {
                tag: tag.to_owned(),
                version: version.to_owned(),
            });
        }
    }
    feed
}

/// Whether a dotted version like `0.2.0` reads as one.
fn parses(version: &str) -> bool {
    crate::gate::parse(version).is_some()
}

/// Whether `latest` is a step ahead of the version running now. A
/// current version that does not parse, such as a dev build, counts
/// as behind.
#[must_use]
pub fn is_newer(latest: &str, current: &str) -> bool {
    crate::gate::is_below(current, latest)
}

/// Whether a version kage reports, such as `0.3.1 (abc1234)`, is
/// behind release `latest`. A nightly is not: it has no release number
/// to compare, and the nightly channel tracks it by commit instead.
#[must_use]
pub fn reported_behind(latest: &str, reported: &str) -> bool {
    kage_core::build::release(reported)
        .is_some_and(|number| parses(number) && is_newer(latest, number))
}

/// What the last check saw as out of date, as one summary line for
/// the toast and the settings badge, or `None` when nothing is
/// behind. The stable channel compares parsed semvers: a client
/// behind the release names the release, whose install brings its
/// engine too, and an engine run from elsewhere is named on its own.
/// The nightly channel compares this client's build commit against
/// the nightly's, since an engine's nightly identity is not its
/// version. A version that does not parse, such as a dev build, never
/// counts as behind.
#[must_use]
pub fn pending_update(prefs: &crate::prefs::Prefs, engine: Option<&str>) -> Option<String> {
    let behind = match prefs.channel {
        crate::prefs::Channel::Nightly => {
            let build = prefs.latest_nightly.as_ref()?;
            // Only a CI build is on some nightly; a local build names
            // its checkout's commit and would read as behind forever.
            let sha = option_env!("KAGE_BUILD_SHA")?;
            (sha != build.commit).then(|| format!("desktop nightly {}", build.date))
        }
        crate::prefs::Channel::Latest => {
            let latest = prefs.latest_version.as_deref()?;
            if reported_behind(latest, env!("KAGE_VERSION")) {
                Some(format!("kage {latest}"))
            } else {
                engine
                    .filter(|now| reported_behind(latest, now))
                    .map(|_| format!("engine {latest}"))
            }
        }
    }?;
    Some(format!("{behind} is available"))
}

/// The engine release archive built for `os` and `arch`, named the
/// way the release workflow uploads it. `None` where no release is
/// built.
#[must_use]
pub fn cli_asset_for(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") => Some("kage-x86_64-linux.tar.xz"),
        ("linux", "aarch64") => Some("kage-aarch64-linux.tar.xz"),
        ("macos", "x86_64") => Some("kage-x86_64-macos.tar.xz"),
        ("macos", "aarch64") => Some("kage-aarch64-macos.tar.xz"),
        ("windows", "x86_64") => Some("kage-x86_64-windows.zip"),
        _ => None,
    }
}

/// The raw engine artifact built for the platform running this code.
#[must_use]
pub fn cli_asset() -> Option<&'static str> {
    cli_asset_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// The desktop release archive built for `os` and `arch`, named the
/// way the release workflow uploads it. `onelf` picks the
/// portable Linux bundle over the plain archive. `None` where no
/// release is built.
#[must_use]
pub fn desktop_asset_for(os: &str, arch: &str, onelf: bool) -> Option<&'static str> {
    match (os, arch) {
        ("linux", "x86_64") if onelf => Some("kage-desktop-x86_64-linux.onelf"),
        ("linux", "aarch64") if onelf => Some("kage-desktop-aarch64-linux.onelf"),
        ("linux", "x86_64") => Some("kage-desktop-x86_64-linux.tar.xz"),
        ("linux", "aarch64") => Some("kage-desktop-aarch64-linux.tar.xz"),
        ("macos", "x86_64") => Some("kage-desktop-x86_64-macos.tar.xz"),
        ("macos", "aarch64") => Some("kage-desktop-aarch64-macos.tar.xz"),
        ("windows", "x86_64") => Some("kage-desktop-x86_64-windows.zip"),
        _ => None,
    }
}

/// The desktop artifact this running build updates with. A Linux
/// binary that carries an onelf footer swaps for the portable
/// bundle, whose bundled libraries the plain archive build lacks;
/// anything else swaps for the plain archive. The footer, not the
/// file name, tells the two apart, so a renamed bundle keeps
/// updating as the bundle it is.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn desktop_asset() -> Option<&'static str> {
    let onelf =
        cfg!(target_os = "linux") && std::env::current_exe().is_ok_and(|exe| is_onelf_bundle(&exe));
    desktop_asset_for(std::env::consts::OS, std::env::consts::ARCH, onelf)
}

/// Whether `path` is a packed onelf bundle: the file ends in the
/// 76-byte footer the format pins, whose magic every version
/// carries. Anything unreadable, shorter, or plain reads as not a
/// bundle.
#[cfg(not(target_arch = "wasm32"))]
fn is_onelf_bundle(path: &std::path::Path) -> bool {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut footer = [0; 76];
    if file
        .seek(SeekFrom::End(-76))
        .and_then(|_| file.read_exact(&mut footer))
        .is_err()
    {
        return false;
    }
    footer[..8] == *b"ONELF\0\x01\x00" && footer[68..] == *b"FLENONE\0"
}

/// The download URL for one release artifact.
#[must_use]
pub fn asset_url(tag: &str, asset: &str) -> String {
    format!("{RELEASES_DOWNLOAD}/{tag}/{asset}")
}

/// Where an installed engine lands, for the setup screen's caption.
#[must_use]
pub fn install_hint() -> &'static str {
    if cfg!(windows) {
        "Saved to %LOCALAPPDATA%\\Programs\\kage and wired into the app."
    } else {
        "Saved to ~/.local/bin and wired into the app."
    }
}

/// Opens the channel's releases page in the user's browser. A no-op
/// on the web build, where the page is one tab away already.
pub fn open_releases() {
    #[cfg(not(target_arch = "wasm32"))]
    crate::external::open(releases_page(crate::prefs::load().channel).as_ref());
}

/// Downloads the newest engine release of the saved channel for this
/// platform, extracts the engine binary and installs it where the
/// user's account owns it, returning the binary's path. The web
/// build reports the platform as unable.
pub fn install_latest() -> Result<std::path::PathBuf, String> {
    #[cfg(not(target_arch = "wasm32"))]
    return native::install_latest(crate::prefs::load().channel);

    #[cfg(target_arch = "wasm32")]
    Err("this platform cannot download kage".to_owned())
}

/// Downloads the newest desktop release of the saved channel for
/// this platform and swaps the running app and the engine beside it
/// for it, returning where the new install sits. A swap takes effect
/// at the next launch; the running app keeps its own binary. The web
/// build reports the platform as unable.
pub fn install_desktop_latest() -> Result<std::path::PathBuf, String> {
    #[cfg(not(target_arch = "wasm32"))]
    return native::install_desktop_latest(crate::prefs::load().channel);

    #[cfg(target_arch = "wasm32")]
    Err("this platform cannot download kage".to_owned())
}

/// What one release check saw, for the UI thread to apply.
#[derive(Debug, Default)]
pub struct CheckOutcome {
    /// The newest release, without the `v`.
    pub latest: Option<String>,
    /// The newest nightly, when the channel is nightly.
    pub nightly: Option<crate::prefs::NightlyBuild>,
    /// Why the check failed, when it did. Cleared on a success.
    pub error: Option<String>,
    /// Whether a newer engine or client is out, for the badge.
    pub badge: bool,
    /// Unix seconds of the completed check.
    pub checked_at: i64,
}

/// Runs one release check when one is due and returns what it saw.
/// `force` skips the daily throttle, for the button. The web build
/// never checks. The caller runs it on its own thread and applies the
/// outcome on the UI thread, so the store never crosses the boundary.
pub fn run_check(prefs: &crate::prefs::Prefs, force: bool) -> Option<CheckOutcome> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (prefs, force);
        None
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        if !due_for_check(prefs, force) {
            return None;
        }
        let channel = prefs.channel;
        let text = match native::fetch_releases() {
            Ok(text) => text,
            Err(error) => {
                return Some(CheckOutcome {
                    error: Some(error),
                    ..Default::default()
                });
            }
        };
        let feed = parse_feed(&text);
        let nightly = match channel {
            crate::prefs::Channel::Latest => None,
            crate::prefs::Channel::Nightly => {
                let commit = native::fetch_nightly_commit();
                match (commit, feed.nightly.as_ref()) {
                    (Ok(Some(commit)), Some(release)) => Some(crate::prefs::NightlyBuild {
                        commit,
                        date: release.date.clone(),
                    }),
                    _ => None,
                }
            }
        };
        if feed.latest.is_none() && nightly.is_none() {
            return Some(CheckOutcome {
                error: Some("the release list named nothing".to_owned()),
                ..Default::default()
            });
        }
        Some(CheckOutcome {
            latest: feed.latest.map(|release| release.version),
            nightly,
            error: None,
            badge: false,
            checked_at: crate::clock::unix_seconds(),
        })
    }
}

/// Applies a check outcome to the preferences. The badge needs the
/// engine's own version, which the caller reads off the store.
pub fn apply_outcome(prefs: &mut crate::prefs::Prefs, outcome: CheckOutcome, engine: Option<&str>) {
    if let Some(version) = outcome.latest {
        prefs.latest_version = Some(version);
    }
    if outcome.nightly.is_some() {
        prefs.latest_nightly = outcome.nightly;
    }
    prefs.check_error = outcome.error;
    prefs.update_checked_at = Some(outcome.checked_at);
    prefs.update_badge = pending_update(prefs, engine).is_some();
}

/// Whether enough time has passed since the last completed check.
#[cfg(not(target_arch = "wasm32"))]
fn due_for_check(prefs: &crate::prefs::Prefs, force: bool) -> bool {
    force
        || prefs
            .update_checked_at
            .is_none_or(|at| at.saturating_add(CHECK_INTERVAL) <= crate::clock::unix_seconds())
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::io::Read as _;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    /// The client the GitHub API asks for; it requires a user agent.
    fn agent() -> ureq::Agent {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(15))
            .user_agent(concat!("kage-desktop/", env!("CARGO_PKG_VERSION")))
            .build()
    }

    /// The message for a failed release fetch. Releases older than
    /// the raw-binary upload carry only archives, so a missing
    /// artifact is expected there and should not read as HTTP jargon.
    pub(super) fn did_not_load(error: ureq::Error, what: &str) -> String {
        if matches!(error, ureq::Error::Status(404, _)) {
            return format!("{what} was not found");
        }
        format!("{what} did not load: {error}")
    }

    /// Gets a URL's body as text.
    fn fetch_text(url: &str, what: &str) -> Result<String, String> {
        agent()
            .get(url)
            .call()
            .map_err(|error| did_not_load(error, what))?
            .into_string()
            .map_err(|error| format!("{what} did not read: {error}"))
    }

    /// Gets a URL's body as bytes, refusing past `cap`. `None` when
    /// the release names no such artifact.
    fn fetch_bytes_opt(url: &str, cap: usize, what: &str) -> Result<Option<Vec<u8>>, String> {
        let response = match agent().get(url).call() {
            Ok(response) => response,
            Err(ureq::Error::Status(404, _)) => return Ok(None),
            Err(error) => return Err(did_not_load(error, what)),
        };
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(cap as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("{what} did not read: {error}"))?;
        Ok(Some(bytes))
    }

    /// Gets a URL's body as bytes, refusing past `cap`.
    fn fetch_bytes(url: &str, cap: usize, what: &str) -> Result<Vec<u8>, String> {
        fetch_bytes_opt(url, cap, what)?.ok_or_else(|| format!("{what} was not found"))
    }

    /// Fetches the releases list as text.
    pub(super) fn fetch_releases() -> Result<String, String> {
        fetch_text(super::RELEASES_API, "the release list")
    }

    /// The commit the `nightly` tag points at, when it exists.
    pub(super) fn fetch_nightly_commit() -> Result<Option<String>, String> {
        let text = fetch_text(super::NIGHTLY_REF_API, "the nightly tag")?;
        let value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| format!("the nightly tag did not read: {error}"))?;
        Ok(value["object"]["sha"].as_str().map(str::to_owned))
    }

    /// Downloads the newest engine release of `channel` and installs
    /// the engine binary it contains, returning the installed path.
    pub(super) fn install_latest(channel: crate::prefs::Channel) -> Result<PathBuf, String> {
        let Some(archive) = super::cli_asset() else {
            return Err("no kage release is built for this platform".to_owned());
        };
        let tag = match channel {
            crate::prefs::Channel::Latest => {
                let feed = super::parse_feed(&fetch_releases()?);
                feed.latest.ok_or("the release list names no release")?.tag
            }
            // The rolling tag exists once the first nightly published.
            crate::prefs::Channel::Nightly => "nightly".to_owned(),
        };
        let url = super::asset_url(&tag, archive);
        let bytes = fetch_bytes(&url, super::BINARY_CAP, "the kage archive")?;
        install(archive, &bytes)
    }

    /// Downloads the newest desktop release of `channel` and swaps
    /// the running app for it, returning where the new install sits.
    pub(super) fn install_desktop_latest(
        channel: crate::prefs::Channel,
    ) -> Result<PathBuf, String> {
        let Some(archive) = super::desktop_asset() else {
            return Err("no kage desktop release is built for this platform".to_owned());
        };
        let tag = match channel {
            crate::prefs::Channel::Latest => {
                let feed = super::parse_feed(&fetch_releases()?);
                feed.latest.ok_or("the release list names no release")?.tag
            }
            crate::prefs::Channel::Nightly => "nightly".to_owned(),
        };
        let url = super::asset_url(&tag, archive);
        let bytes = fetch_bytes(&url, super::BINARY_CAP, "the kage desktop archive")?;
        let scratch = scratch_dir()?;
        let swapped =
            stage_desktop(archive, &bytes, &scratch).and_then(|payload| swap_into_place(&payload));
        let _ = std::fs::remove_dir_all(&scratch);
        swapped
    }

    /// Unpacks the desktop archive in `scratch` and returns the
    /// payload to install: the app bundle on macOS, the app binary
    /// elsewhere. The `.onelf` bundle is the download itself and only
    /// needs writing out.
    fn stage_desktop(archive: &str, bytes: &[u8], scratch: &Path) -> Result<PathBuf, String> {
        if archive.ends_with(".onelf") {
            let path = scratch.join(archive);
            std::fs::write(&path, bytes)
                .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
            }
            return Ok(path);
        }
        let archive_path = scratch.join(archive);
        std::fs::write(&archive_path, bytes)
            .map_err(|error| format!("cannot write {}: {error}", archive_path.display()))?;
        compak::extract_archive(archive_path.as_path(), scratch)
            .map_err(|error| format!("cannot unpack {archive}: {error}"))?;
        find_desktop_payload(scratch)
    }

    /// The extracted desktop payload: the `Kage.app` bundle on macOS,
    /// the `kage-desktop` binary elsewhere. Release archives nest the
    /// folder one level, so the search walks.
    pub(super) fn find_desktop_payload(dir: &Path) -> Result<PathBuf, String> {
        let wanted = if cfg!(windows) {
            "kage-desktop.exe"
        } else {
            "kage-desktop"
        };
        let mut found = None;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(next) = stack.pop() {
            let entries = std::fs::read_dir(&next)
                .map_err(|error| format!("cannot read {}: {error}", next.display()))?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if cfg!(target_os = "macos")
                        && path.file_name().is_some_and(|name| name == "Kage.app")
                    {
                        return Ok(path);
                    }
                    stack.push(path);
                } else if path.file_name().is_some_and(|name| name == wanted) {
                    found = Some(path);
                }
            }
        }
        found.ok_or_else(|| format!("the archive holds no {wanted}"))
    }

    /// Swaps the running app for the staged payload, and the engine
    /// beside it for the one staged beside the payload, and returns
    /// where the new install sits. A macOS install swaps the whole
    /// bundle, and the onelf bundle carries its engine inside.
    fn swap_into_place(staged: &Path) -> Result<PathBuf, String> {
        let current = std::env::current_exe()
            .map_err(|error| format!("cannot locate the running app: {error}"))?;
        #[cfg(target_os = "macos")]
        if let Some(app) = app_bundle_of(&current) {
            return swap_bundle(&app, staged);
        }
        let engine = format!("kage{}", std::env::consts::EXE_SUFFIX);
        let staged_engine = staged.with_file_name(&engine);
        swap_file(staged, &current)
            .map_err(|error| format!("cannot move the new app into place: {error}"))?;
        if staged_engine.is_file() {
            swap_file(&staged_engine, &current.with_file_name(&engine))
                .map_err(|error| format!("cannot move the new engine into place: {error}"))?;
        }
        Ok(current)
    }

    /// Puts `staged` where `target` is. A running binary cannot be
    /// overwritten on Windows but can be renamed, so the old one steps
    /// aside first and the new one takes its place at the next launch;
    /// on Unix the rename replaces it in place while a running process
    /// keeps its own copy.
    fn swap_file(staged: &Path, target: &Path) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            let mut aside = target.as_os_str().to_owned();
            aside.push(".old");
            let aside = PathBuf::from(aside);
            let _ = std::fs::remove_file(&aside);
            let moved = target.exists();
            if moved {
                std::fs::rename(target, &aside)?;
            }
            if let Err(error) = std::fs::copy(staged, target) {
                if moved {
                    let _ = std::fs::rename(&aside, target);
                }
                return Err(error);
            }
            Ok(())
        }
        #[cfg(not(windows))]
        std::fs::rename(staged, target)
    }

    /// The `.app` bundle `exe` runs from, when it sits in one.
    #[cfg(target_os = "macos")]
    fn app_bundle_of(exe: &Path) -> Option<PathBuf> {
        exe.parent()
            .and_then(|macos| macos.parent())
            .and_then(|contents| contents.parent())
            .filter(|app| app.extension().is_some_and(|ext| ext == "app"))
            .map(Path::to_path_buf)
    }

    /// Moves the old bundle aside and the staged one in. The old copy
    /// stays until the next update, so a rollback is a rename away.
    #[cfg(target_os = "macos")]
    fn swap_bundle(app: &Path, staged: &Path) -> Result<PathBuf, String> {
        let aside = app.with_extension("app.old");
        let _ = std::fs::remove_dir_all(&aside);
        std::fs::rename(app, &aside)
            .map_err(|error| format!("cannot move the old app aside: {error}"))?;
        if let Err(error) = std::fs::rename(staged, app) {
            let _ = std::fs::rename(&aside, app);
            return Err(format!("cannot move the new app into place: {error}"));
        }
        Ok(app.to_path_buf())
    }

    /// Unpacks the archive in a scratch directory and installs the
    /// engine binary it contains under the user's own bin, returning
    /// the installed path.
    fn install(archive: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        let scratch = scratch_dir()?;
        let dir = install_dir()?;
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let name = if cfg!(windows) { "kage.exe" } else { "kage" };
        let staging = dir.join(format!("{name}.new"));
        let staged = stage_binary(archive, bytes, &scratch, &staging);
        let _ = std::fs::remove_dir_all(&scratch);
        staged?;
        let target = staging.with_file_name(name);
        if target.exists() {
            let _ = std::fs::remove_file(&target);
        }
        std::fs::rename(&staging, &target)
            .map_err(|error| format!("cannot move it into place: {error}"))?;
        Ok(target)
    }

    /// Writes the archive to `scratch`, unpacks it and copies the
    /// engine binary to `staging`, executable bit set.
    pub(super) fn stage_binary(
        archive: &str,
        bytes: &[u8],
        scratch: &Path,
        staging: &Path,
    ) -> Result<(), String> {
        let archive_path = scratch.join(archive);
        std::fs::write(&archive_path, bytes)
            .map_err(|error| format!("cannot write {}: {error}", archive_path.display()))?;
        compak::extract_archive(archive_path.as_path(), scratch)
            .map_err(|error| format!("cannot unpack {archive}: {error}"))?;
        let binary = find_engine_binary(scratch)?;
        std::fs::copy(&binary, staging)
            .map_err(|error| format!("cannot stage {}: {error}", staging.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(staging, std::fs::Permissions::from_mode(0o755));
        }
        Ok(())
    }

    /// The extracted engine binary: the one file named `kage` (plus
    /// the Windows suffix) under the unpacked archive. Release
    /// archives nest it under a target-named directory, so the search
    /// walks instead of assuming the layout.
    pub(super) fn find_engine_binary(dir: &Path) -> Result<PathBuf, String> {
        let wanted = if cfg!(windows) { "kage.exe" } else { "kage" };
        let mut stack = vec![dir.to_path_buf()];
        while let Some(next) = stack.pop() {
            let entries = std::fs::read_dir(&next)
                .map_err(|error| format!("cannot read {}: {error}", next.display()))?;
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.file_name().is_some_and(|name| name == wanted) {
                    return Ok(path);
                }
            }
        }
        Err(format!("the archive holds no {wanted}"))
    }

    /// A fresh scratch directory for one download and unpack.
    pub(super) fn scratch_dir() -> Result<PathBuf, String> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir = std::env::temp_dir().join(format!("kage-install-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        Ok(dir)
    }

    /// The user-owned directory an installed engine goes to.
    fn install_dir() -> Result<PathBuf, String> {
        if cfg!(windows) {
            std::env::var_os("LOCALAPPDATA")
                .map(|base| PathBuf::from(base).join("Programs").join("kage"))
                .ok_or_else(|| "LOCALAPPDATA is not set".to_owned())
        } else {
            std::env::var_os("HOME")
                .map(|base| PathBuf::from(base).join(".local").join("bin"))
                .ok_or_else(|| "HOME is not set".to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_release_tag_wins_past_old_desktop_tags() {
        let feed = parse_feed(
            r#"[
                {"tag_name": "kage-desktop-v0.9.0"},
                {"tag_name": "v0.2.0"},
                {"tag_name": "v0.1.0"},
                {"tag_name": "kage-desktop-v0.8.0"}
            ]"#,
        );
        assert_eq!(
            feed.latest.map(|release| release.tag),
            Some("v0.2.0".to_owned())
        );
    }

    #[test]
    fn unparsable_tags_are_skipped() {
        let feed = parse_feed(
            r#"[
                {"tag_name": "v0.3.0-rc.1"},
                {"tag_name": "v0.2.0"}
            ]"#,
        );
        assert_eq!(
            feed.latest.map(|release| release.version),
            Some("0.2.0".to_owned())
        );
    }

    #[test]
    fn a_garbage_feed_names_nothing() {
        assert_eq!(parse_feed("not json"), Feed::default());
        assert_eq!(parse_feed("{}"), Feed::default());
        assert_eq!(parse_feed("[]"), Feed::default());
    }

    #[test]
    fn the_nightly_release_names_its_date() {
        let feed = parse_feed(
            r#"[
                {"tag_name": "nightly", "published_at": "2026-10-10T03:04:05Z"},
                {"tag_name": "v0.2.1"}
            ]"#,
        );
        assert_eq!(
            feed.nightly.map(|nightly| nightly.date),
            Some("2026-10-10".to_owned())
        );
        assert_eq!(
            feed.latest.map(|release| release.version),
            Some("0.2.1".to_owned())
        );
    }

    #[test]
    fn a_nightly_without_a_date_names_nothing() {
        let feed = parse_feed(r#"[{"tag_name": "nightly"}]"#);
        assert_eq!(feed.nightly, None);
    }

    #[test]
    fn each_channel_reads_its_own_releases_page() {
        use crate::prefs::Channel;
        assert_eq!(
            releases_page(Channel::Latest),
            "https://github.com/QaidVoid/kage/releases/latest"
        );
        assert_eq!(
            releases_page(Channel::Nightly),
            "https://github.com/QaidVoid/kage/releases/tags/nightly"
        );
    }

    #[test]
    fn a_reported_version_is_behind_by_its_release_number() {
        assert!(reported_behind("0.4.0", "0.3.1 (abc1234)"));
        assert!(!reported_behind("0.3.1", "0.3.1 (abc1234)"));
        assert!(!reported_behind("9.9.9", "nightly (abc1234)"));
        assert!(!reported_behind("0.4.0", "dev"));
    }

    #[test]
    fn newer_compares_dotted_parts() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.2.0", "0.2.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.2.0", "dev"));
    }

    #[test]
    fn every_release_target_maps_to_an_asset() {
        for (os, arch, asset) in [
            ("linux", "x86_64", "kage-x86_64-linux.tar.xz"),
            ("linux", "aarch64", "kage-aarch64-linux.tar.xz"),
            ("macos", "x86_64", "kage-x86_64-macos.tar.xz"),
            ("macos", "aarch64", "kage-aarch64-macos.tar.xz"),
            ("windows", "x86_64", "kage-x86_64-windows.zip"),
        ] {
            assert_eq!(cli_asset_for(os, arch), Some(asset));
        }
        assert_eq!(cli_asset_for("plan9", "x86_64"), None);
        assert_eq!(cli_asset_for("linux", "riscv64"), None);
    }

    #[test]
    fn every_desktop_target_maps_to_an_asset() {
        assert_eq!(
            desktop_asset_for("linux", "x86_64", false),
            Some("kage-desktop-x86_64-linux.tar.xz")
        );
        assert_eq!(
            desktop_asset_for("linux", "x86_64", true),
            Some("kage-desktop-x86_64-linux.onelf")
        );
        assert_eq!(
            desktop_asset_for("linux", "aarch64", true),
            Some("kage-desktop-aarch64-linux.onelf")
        );
        assert_eq!(
            desktop_asset_for("macos", "aarch64", false),
            Some("kage-desktop-aarch64-macos.tar.xz")
        );
        assert_eq!(
            desktop_asset_for("windows", "x86_64", false),
            Some("kage-desktop-x86_64-windows.zip")
        );
        assert_eq!(desktop_asset_for("plan9", "x86_64", false), None);
    }

    #[test]
    fn pending_update_names_what_is_behind_on_stable() {
        use crate::prefs::Channel;
        let mut prefs = crate::prefs::Prefs {
            channel: Channel::Latest,
            ..crate::prefs::Prefs::default()
        };
        assert_eq!(pending_update(&prefs, None), None);

        prefs.latest_version = Some("99.0.0".to_owned());
        assert_eq!(
            pending_update(&prefs, Some("0.1.0")).as_deref(),
            Some("kage 99.0.0 is available"),
            "the release brings its engine, so it is named once"
        );

        // A release no newer than this client, with an older engine
        // run from elsewhere, names the engine alone.
        prefs.latest_version = Some(env!("CARGO_PKG_VERSION").to_owned());
        assert_eq!(
            pending_update(&prefs, Some("0.0.1")).as_deref(),
            Some(format!("engine {} is available", env!("CARGO_PKG_VERSION")).as_str())
        );
        assert_eq!(
            pending_update(&prefs, None),
            None,
            "an engine whose version is unknown is not counted behind"
        );
        prefs.latest_version = Some("0.0.1".to_owned());
        assert_eq!(pending_update(&prefs, Some("0.1.0")), None);
    }

    #[test]
    fn a_dev_build_never_reads_as_behind_on_nightly() {
        use crate::prefs::{Channel, NightlyBuild};
        let prefs = crate::prefs::Prefs {
            channel: Channel::Nightly,
            latest_nightly: Some(NightlyBuild {
                commit: "0123abcd".to_owned(),
                date: "2026-10-10".to_owned(),
            }),
            ..crate::prefs::Prefs::default()
        };
        // A dev build carries no KAGE_BUILD_SHA, so the comparison
        // stays silent instead of toasting on every launch.
        assert_eq!(pending_update(&prefs, None), None);
    }

    #[test]
    fn asset_urls_point_at_the_release_download() {
        assert_eq!(
            asset_url("v0.2.0", "kage-aarch64-macos.tar.xz"),
            "https://github.com/QaidVoid/kage/releases/download/v0.2.0/kage-aarch64-macos.tar.xz"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_missing_artifact_does_not_read_as_http_jargon() {
        use super::native::did_not_load;
        let missing = ureq::Error::Status(404, ureq::Response::new(404, "Not Found", "").unwrap());
        assert_eq!(
            did_not_load(missing, "the kage binary"),
            "the kage binary was not found"
        );
        let broken = ureq::Error::Status(500, ureq::Response::new(500, "Nope", "").unwrap());
        assert!(did_not_load(broken, "the kage binary").contains("did not load"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_engine_is_found_under_the_release_archive_layout() {
        use super::native::{find_engine_binary, scratch_dir};
        let scratch = scratch_dir().unwrap();
        let nested = scratch.join("kage-x86_64-linux");
        let engine = format!("kage{}", std::env::consts::EXE_SUFFIX);
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join(&engine), "binary").unwrap();
        std::fs::write(scratch.join("LICENSE"), "MIT").unwrap();
        let found = find_engine_binary(&scratch).unwrap();
        assert_eq!(found, nested.join(&engine));
        let _ = std::fs::remove_dir_all(&scratch);

        let empty = scratch_dir().unwrap();
        let missing = find_engine_binary(&empty).unwrap_err();
        assert!(missing.contains("holds no kage"), "got {missing}");
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    #[test]
    fn the_desktop_payload_is_found_under_the_release_layouts() {
        use super::native::{find_desktop_payload, scratch_dir};

        let scratch = scratch_dir().unwrap();
        let folder = scratch.join("kage-desktop-x86_64-linux");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("kage-desktop"), "binary").unwrap();
        std::fs::write(folder.join("LICENSE"), "MIT").unwrap();
        let found = find_desktop_payload(&scratch).unwrap();
        assert_eq!(found, folder.join("kage-desktop"));
        let _ = std::fs::remove_dir_all(&scratch);

        // The bundle-wins rule is a macOS one; the macOS archives are
        // the only ones that ship a Kage.app.
        #[cfg(target_os = "macos")]
        {
            let scratch = scratch_dir().unwrap();
            let macos = scratch.join("kage-desktop-aarch64-macos/Kage.app/Contents/MacOS");
            std::fs::create_dir_all(&macos).unwrap();
            std::fs::write(macos.join("kage-desktop"), "binary").unwrap();
            let found = find_desktop_payload(&scratch).unwrap();
            assert_eq!(found.file_name().unwrap(), "Kage.app");
            let _ = std::fs::remove_dir_all(&scratch);
        }

        let empty = scratch_dir().unwrap();
        let missing = find_desktop_payload(&empty).unwrap_err();
        assert!(missing.contains("holds no kage-desktop"), "got {missing}");
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    #[test]
    fn a_linux_bundle_is_known_by_its_footer_not_its_name() {
        use super::is_onelf_bundle;
        use std::io::Write as _;

        let dir = super::native::scratch_dir().unwrap();
        let mut bundle = std::fs::File::create(dir.join("kage-desktop")).unwrap();
        bundle.write_all(b"\x7fELF runtime body").unwrap();
        bundle.write_all(&[0; 30]).unwrap();
        let mut footer = [0; 76];
        footer[..8].copy_from_slice(b"ONELF\0\x01\x00");
        footer[68..].copy_from_slice(b"FLENONE\0");
        bundle.write_all(&footer).unwrap();
        drop(bundle);
        assert!(is_onelf_bundle(&dir.join("kage-desktop")));
        std::fs::rename(dir.join("kage-desktop"), dir.join("renamed")).unwrap();
        assert!(
            is_onelf_bundle(&dir.join("renamed")),
            "a rename must not change what the file updates with"
        );

        let plain = dir.join("plain");
        std::fs::write(&plain, b"\x7fELF plain binary, no footer").unwrap();
        assert!(!is_onelf_bundle(&plain));
        assert!(!is_onelf_bundle(&plain), "repeat reads agree");
        let _ = std::fs::remove_dir_all(&dir);

        // A real bundle of ours, when one sits on this machine,
        // pins the reader to the shipped format.
        if let Some(home) = std::env::var_os("HOME") {
            let candidate =
                std::path::Path::new(&home).join("Downloads/kage-desktop-x86_64-linux.onelf");
            if candidate.is_file() {
                assert!(
                    is_onelf_bundle(&candidate),
                    "the shipped bundle {} must read as a bundle",
                    candidate.display()
                );
            }
        }
    }

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    #[test]
    fn a_release_archive_unpacks_and_yields_the_engine() {
        use super::native::{scratch_dir, stage_binary};
        let source = scratch_dir().unwrap();
        let payload = source.join("kage-x86_64-linux");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::write(payload.join("kage"), "#!/bin/sh\necho ok\n").unwrap();
        let archive = source.join("kage-x86_64-linux.tar.xz");
        let built = std::process::Command::new("tar")
            .args([
                "-cJf",
                archive.to_str().unwrap(),
                "-C",
                source.to_str().unwrap(),
                "kage-x86_64-linux",
            ])
            .status()
            .expect("system tar");
        assert!(built.success(), "tar -cJf must build the test archive");

        let out = scratch_dir().unwrap();
        let staging = out.join("kage.new");
        stage_binary(
            "kage-x86_64-linux.tar.xz",
            &std::fs::read(&archive).unwrap(),
            &out,
            &staging,
        )
        .unwrap();
        let staged = std::fs::read_to_string(&staging).unwrap();
        assert_eq!(staged, "#!/bin/sh\necho ok\n");
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&staging).unwrap().permissions().mode() & 0o111,
            0o111,
            "the staged binary must be executable"
        );
        let _ = std::fs::remove_dir_all(&source);
        let _ = std::fs::remove_dir_all(&out);
    }
}
