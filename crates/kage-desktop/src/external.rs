//! Hands things to the platform: a URL or file to the system's default
//! app, and an audio clip to whatever plays it. egui draws no player,
//! so the native build writes a clip to a temp file and opens it, and
//! the browser build plays it through an audio element.

/// Opens `target`, a URL or a file path, with the system's default app
/// and does not wait for it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn open(target: &std::ffi::OsStr) {
    let opened = if cfg!(windows) {
        std::process::Command::new("cmd")
            .args([
                std::ffi::OsStr::new("/C"),
                "start".as_ref(),
                "".as_ref(),
                target,
            ])
            .spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(target).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(target).spawn()
    };
    if let Err(error) = opened {
        crate::warn(&format!("{} did not open: {error}", target.display()));
    }
}

/// Plays the clip whose bytes are `data` in base64, of type `mime`,
/// with the system's default player. The file is named by the clip's
/// content, so playing it again reuses it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn play(mime: &str, data: &str) {
    use base64::Engine as _;
    use sha2::Digest as _;

    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
        crate::warn("the clip's data is damaged, so it was not played");
        return;
    };
    let digest = hex::encode(sha2::Sha256::digest(&bytes));
    let extension = kage_core::audio::extension_for(mime).unwrap_or("bin");
    let path = std::env::temp_dir().join(format!("kage-clip-{}.{extension}", &digest[..16]));
    if let Err(error) = std::fs::write(&path, &bytes) {
        crate::warn(&format!("cannot write {}: {error}", path.display()));
        return;
    }
    open(path.as_os_str());
}

/// Plays the clip whose bytes are `data` in base64, of type `mime`,
/// through an audio element the page does not show.
#[cfg(target_arch = "wasm32")]
pub(crate) fn play(mime: &str, data: &str) {
    let src = format!("data:{mime};base64,{data}");
    match web_sys::HtmlAudioElement::new_with_src(&src) {
        Ok(audio) => {
            if let Err(error) = audio.play() {
                crate::warn(&format!("the clip did not play: {error:?}"));
            }
        }
        Err(error) => crate::warn(&format!("the clip did not play: {error:?}")),
    }
}
