//! Writing the chosen channel into the workspace's toolchain file.

use std::path::{Path, PathBuf};

use super::SyncError;
use crate::parse_toolchain_channel;

/// Pin `channel` for `cwd`: update the nearest `rust-toolchain.toml` /
/// `rust-toolchain` (as rustup resolves it), or create
/// `<cwd>/rust-toolchain.toml` when the directory is unpinned. Returns the
/// file written.
pub fn write_pin(cwd: &Path, channel: &str) -> Result<PathBuf, SyncError> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source| SyncError::PinIo { path, source }
    };
    let Some(path) = rbs_toolchain::find_toolchain_file(cwd) else {
        let path = cwd.join("rust-toolchain.toml");
        std::fs::write(&path, set_channel("", channel)?).map_err(io(&path))?;
        return Ok(path);
    };
    let text = std::fs::read_to_string(&path).map_err(io(&path))?;
    let new = if is_legacy_plain(&path, &text) {
        format!("{channel}\n")
    } else {
        set_channel(&text, channel)?
    };
    std::fs::write(&path, new).map_err(io(&path))?;
    Ok(path)
}

/// The channel pinned for `cwd`, if any: the nearest toolchain file's
/// `[toolchain] channel`, or the single line of a legacy plain
/// `rust-toolchain`. `None` when unpinned, unreadable, or pinned to a `path`.
pub fn pinned_channel(cwd: &Path) -> Option<String> {
    let path = rbs_toolchain::find_toolchain_file(cwd)?;
    let text = std::fs::read_to_string(&path).ok()?;
    if is_legacy_plain(&path, &text) {
        Some(text.trim().to_string()).filter(|c| !c.is_empty())
    } else {
        parse_toolchain_channel(&text)
    }
}

/// A legacy `rust-toolchain` holding just a channel name rather than TOML.
fn is_legacy_plain(path: &Path, text: &str) -> bool {
    path.file_name().is_some_and(|n| n == "rust-toolchain")
        && text.trim().lines().count() <= 1
        && !text.contains('[')
        && !text.contains('=')
}

/// Set `[toolchain] channel` in TOML `text`, leaving every other line as is.
/// The result is re-parsed; anything that does not read back as `channel` is
/// refused rather than written.
pub fn set_channel(text: &str, channel: &str) -> Result<String, SyncError> {
    let line = format!("channel = \"{channel}\"");
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let header = lines.iter().position(|l| l.trim() == "[toolchain]");
    match header {
        Some(h) => {
            let end = lines[h + 1..]
                .iter()
                .position(|l| l.trim_start().starts_with('['))
                .map_or(lines.len(), |i| h + 1 + i);
            let existing = (h + 1..end).find(|&i| {
                lines[i]
                    .trim_start()
                    .strip_prefix("channel")
                    .is_some_and(|rest| rest.trim_start().starts_with('='))
            });
            match existing {
                Some(i) => lines[i] = line,
                None => lines.insert(h + 1, line),
            }
        }
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push("[toolchain]".to_string());
            lines.push(line);
        }
    }
    let mut out = lines.join("\n");
    out.push('\n');
    if out.starts_with('\n') {
        out.remove(0);
    }
    if parse_toolchain_channel(&out).as_deref() == Some(channel) {
        Ok(out)
    } else {
        Err(SyncError::PinEdit {
            channel: channel.to_string(),
        })
    }
}
