//! Updating the launcher in place.
//!
//! The running file is never written to. A new copy is downloaded beside it, checked
//! against the release's `SHA256SUMS`, and renamed over the old path. Linux keeps a
//! running program open by inode, so the process in memory is untouched; the next
//! start is the new build.
//!
//! Windows and macOS cannot replace a running image either, but they can rename it.
//! The same staging step applies there: the install directory or the app bundle is
//! renamed aside, the new one takes its place, and the new build is started.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// A release asset as the GitHub API describes it.
#[derive(Clone, Debug)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|asset| asset.name == name)
    }
}

/// The thing on disk that a newer build replaces.
// Each build only ever constructs the variant for its own platform, so the other two
// look dead to the compiler. They are not.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The AppImage file itself.
    AppImage,
    /// The directory holding the exe, the CEF runtime and the assets.
    WindowsDir,
    /// The `.app` bundle.
    MacBundle,
}

#[derive(Clone, Debug)]
pub struct Target {
    pub kind: Kind,
    /// The file or directory that gets replaced.
    pub path: PathBuf,
    /// Where the download lands before it is checked.
    pub part: PathBuf,
    /// The checked, ready to install copy.
    pub staging: PathBuf,
    /// The previous copy, kept until the new build has run once.
    pub backup: PathBuf,
    /// The asset name this platform wants.
    pub asset: &'static str,
    /// What to start after a restart.
    pub launch: PathBuf,
}

/// `path` with `suffix` appended to the name, not to the extension.
pub fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut text = path.as_os_str().to_os_string();
    text.push(suffix);
    PathBuf::from(text)
}

/// The install this process belongs to, or `None` when it cannot be updated safely.
pub fn target() -> Option<Target> {
    #[cfg(target_os = "linux")]
    {
        // Only an AppImage knows where its own file is. A tarball install is a folder
        // full of CEF, and replacing that in place is a different job.
        let path = PathBuf::from(std::env::var_os("APPIMAGE")?);
        if !path.is_file() {
            return None;
        }
        let dir = path.parent()?.to_path_buf();
        let name = path.file_name()?.to_string_lossy().into_owned();
        let part = dir.join(format!(".{name}.part"));
        let staging = dir.join(format!(".{name}.update"));
        let backup = dir.join(format!(".{name}.old"));
        Some(Target {
            kind: Kind::AppImage,
            launch: path.clone(),
            path,
            part,
            staging,
            backup,
            asset: "pokem-online-x86_64.AppImage",
        })
    }
    #[cfg(target_os = "windows")]
    {
        let exe = std::env::current_exe().ok()?;
        let dir = exe.parent()?.to_path_buf();
        let part = with_suffix(&dir, ".update.zip");
        let staging = with_suffix(&dir, ".update");
        let backup = with_suffix(&dir, ".old");
        Some(Target {
            kind: Kind::WindowsDir,
            launch: dir.join("pokem-online.exe"),
            path: dir,
            part,
            staging,
            backup,
            asset: "pokem-online-windows-x86_64.zip",
        })
    }
    #[cfg(target_os = "macos")]
    {
        let exe = std::env::current_exe().ok()?;
        let bundle = exe
            .ancestors()
            .find(|path| path.extension().map(|ext| ext == "app").unwrap_or(false))?
            .to_path_buf();
        let part = with_suffix(&bundle, ".update.zip");
        let staging = with_suffix(&bundle, ".update");
        let backup = with_suffix(&bundle, ".old");
        Some(Target {
            kind: Kind::MacBundle,
            launch: bundle.clone(),
            path: bundle,
            part,
            staging,
            backup,
            asset: "pokem-online-macos.zip",
        })
    }
}

fn curl() -> Command {
    // The Windows arm below needs the binding mutable; nothing else does.
    #[cfg_attr(not(target_os = "windows"), allow(unused_mut))]
    let mut command = Command::new("curl");
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // No console window behind the launcher.
        command.creation_flags(0x0800_0000);
    }
    command
}

/// A small text response, for the release JSON and the checksum list.
pub fn fetch_text(url: &str) -> Option<String> {
    let output = curl()
        .args([
            "-fsSL",
            "--max-time",
            "20",
            "-H",
            "User-Agent: pokem-online-launcher",
            url,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

pub fn fetch_release(api: &str) -> Option<Release> {
    let text = fetch_text(api)?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let tag = value.get("tag_name")?.as_str()?.to_owned();
    let assets = value
        .get("assets")?
        .as_array()?
        .iter()
        .filter_map(|asset| {
            Some(Asset {
                name: asset.get("name")?.as_str()?.to_owned(),
                url: asset.get("browser_download_url")?.as_str()?.to_owned(),
                size: asset.get("size").and_then(serde_json::Value::as_u64).unwrap_or(0),
            })
        })
        .collect();
    Some(Release { tag, assets })
}

/// The hash for `name` from a `sha256sum` style list.
pub fn parse_checksums(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(file)) = (parts.next(), parts.next()) else {
            continue;
        };
        let file = file.trim_start_matches('*').trim_start_matches("./");
        // The release list may carry a path, but the asset is known by its name.
        let name_only = file.rsplit('/').next().unwrap_or(file);
        if name_only == name && hash.len() == 64 {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

pub fn hex(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(char::from_digit((byte >> 4) as u32, 16).unwrap_or('0'));
        text.push(char::from_digit((byte & 0x0f) as u32, 16).unwrap_or('0'));
    }
    text
}

/// Download with `curl` and report how much of the asset has landed. `total` comes from
/// the release JSON, so no second request is needed to know the size.
pub fn download(
    url: &str,
    dest: &Path,
    total: u64,
    progress: &dyn Fn(u8),
) -> Result<(), String> {
    let _ = std::fs::remove_file(dest);
    let mut child = curl()
        .args([
            "-fL",
            "--retry",
            "2",
            "--connect-timeout",
            "15",
            "--max-time",
            "1800",
            "-o",
        ])
        .arg(dest)
        .arg(url)
        .spawn()
        .map_err(|error| error.to_string())?;

    let mut last = 255u8;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let _ = std::fs::remove_file(dest);
                    return Err(format!("download failed ({status})"));
                }
                break;
            }
            Ok(None) => {
                if total > 0 {
                    let size = std::fs::metadata(dest).map(|meta| meta.len()).unwrap_or(0);
                    let percent = ((size.min(total) * 100) / total) as u8;
                    if percent != last {
                        last = percent;
                        progress(percent);
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(error) => {
                let _ = child.kill();
                return Err(error.to_string());
            }
        }
    }

    let size = std::fs::metadata(dest).map(|meta| meta.len()).unwrap_or(0);
    if total > 0 && size != total {
        let _ = std::fs::remove_file(dest);
        return Err(format!("expected {total} bytes, got {size}"));
    }
    progress(100);
    Ok(())
}

/// Copy a local file, used by the testing hook instead of the network.
pub fn copy(
    source: &Path,
    dest: &Path,
    progress: &dyn Fn(u8),
) -> Result<(), String> {
    let _ = std::fs::remove_file(dest);
    let total = std::fs::metadata(source).map_err(|error| error.to_string())?.len();
    let mut input = std::fs::File::open(source).map_err(|error| error.to_string())?;
    let mut output = std::fs::File::create(dest).map_err(|error| error.to_string())?;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut written = 0u64;
    loop {
        let read = input.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        std::io::Write::write_all(&mut output, &buffer[..read])
            .map_err(|error| error.to_string())?;
        written += read as u64;
        if total > 0 {
            progress(((written.min(total) * 100) / total) as u8);
        }
    }
    progress(100);
    Ok(())
}

/// Move the checked copy into the place the launcher runs from.
pub fn install(target: &Target) -> Result<(), String> {
    if !target.staging.exists() {
        return Err("nothing staged".to_owned());
    }
    remove_any(&target.backup);
    std::fs::rename(&target.path, &target.backup)
        .map_err(|error| format!("could not move the old copy aside: {error}"))?;
    if let Err(error) = std::fs::rename(&target.staging, &target.path) {
        let _ = std::fs::rename(&target.backup, &target.path);
        return Err(format!("could not put the new copy in place: {error}"));
    }
    #[cfg(unix)]
    if target.kind == Kind::AppImage {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&target.path, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Record which build the backup came from, so the next start can tell that the new
/// build actually ran and only then delete the old copy.
pub fn mark_backup(target: &Target) {
    let marker = with_suffix(&target.backup, ".from");
    let _ = std::fs::write(marker, env!("CARGO_PKG_VERSION"));
}

/// Start the installed build. The new process waits for this one to release the
/// single instance lock; `POKEM_WAIT_INSTANCE` is what tells it to.
pub fn spawn(target: &Target) -> Result<(), String> {
    let mut command = match target.kind {
        Kind::MacBundle => {
            let mut command = Command::new("open");
            command.arg("-n").arg(&target.path);
            command
        }
        _ => Command::new(&target.launch),
    };
    command
        .env("POKEM_WAIT_INSTANCE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Unpack a zip into the staging directory. A single top folder in the archive becomes
/// the staging directory itself, which is how both platform zips are built.
pub fn unpack(target: &Target) -> Result<(), String> {
    match target.kind {
        Kind::AppImage => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    &target.part,
                    std::fs::Permissions::from_mode(0o755),
                )
                .map_err(|error| error.to_string())?;
            }
            let _ = std::fs::remove_file(&target.staging);
            std::fs::rename(&target.part, &target.staging)
                .map_err(|error| format!("could not stage the download: {error}"))
        }
        Kind::WindowsDir | Kind::MacBundle => {
            let unpack = with_suffix(&target.staging, ".unpack");
            remove_any(&unpack);
            std::fs::create_dir_all(&unpack).map_err(|error| error.to_string())?;

            let status = match target.kind {
                Kind::WindowsDir => Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        &format!(
                            "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
                            target.part.display(),
                            unpack.display()
                        ),
                    ])
                    .status(),
                _ => Command::new("ditto")
                    .args(["-x", "-k"])
                    .arg(&target.part)
                    .arg(&unpack)
                    .status(),
            }
            .map_err(|error| error.to_string())?;
            if !status.success() {
                remove_any(&unpack);
                return Err(format!("could not unpack the update ({status})"));
            }

            let entries: Vec<PathBuf> = std::fs::read_dir(&unpack)
                .map_err(|error| error.to_string())?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect();
            let root = match entries.as_slice() {
                [only] if only.is_dir() => only.clone(),
                _ => unpack.clone(),
            };
            remove_any(&target.staging);
            std::fs::rename(&root, &target.staging)
                .map_err(|error| format!("could not stage the update: {error}"))?;
            remove_any(&unpack);
            let _ = std::fs::remove_file(&target.part);
            Ok(())
        }
    }
}

/// Delete the copy the previous update left behind, once the new build has run.
pub fn finish_previous(target: &Target) {
    let marker = with_suffix(&target.backup, ".from");
    if !target.backup.exists() {
        return;
    }
    let from = std::fs::read_to_string(&marker).unwrap_or_default();
    if from.trim() == env!("CARGO_PKG_VERSION") {
        // The old build is still the one running, so the swap did not happen.
        return;
    }
    remove_any(&target.backup);
    let _ = std::fs::remove_file(&marker);
}

/// A staged copy that was checked before it was staged. At startup only the AppImage
/// can be checked again cheaply, and only if its hash was recorded.
pub fn staged_hash(target: &Target) -> Option<String> {
    std::fs::read_to_string(with_suffix(&target.staging, ".sha256"))
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
}

pub fn record_hash(target: &Target, hash: &str) {
    let _ = std::fs::write(with_suffix(&target.staging, ".sha256"), hash);
}

pub fn remove_any(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

pub fn sha256_file(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finish())
}

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
    0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
    0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
    0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
    0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
    0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
    0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
    0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
];

/// SHA-256, because the release has to be checked on Windows too and that machine has
/// no `sha256sum`. Small enough to keep here rather than take a dependency for it.
pub struct Sha256 {
    state: [u32; 8],
    buffer: [u8; 64],
    used: usize,
    length: u64,
}

impl Sha256 {
    pub fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c,
                0x1f83d9ab, 0x5be0cd19,
            ],
            buffer: [0; 64],
            used: 0,
            length: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        if self.used > 0 {
            let take = (64 - self.used).min(data.len());
            self.buffer[self.used..self.used + take].copy_from_slice(&data[..take]);
            self.used += take;
            data = &data[take..];
            if self.used == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.used = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.used = data.len();
        }
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.length.wrapping_mul(8);
        self.update(&[0x80]);
        while self.used != 56 {
            self.update(&[0]);
        }
        self.update(&bits.to_be_bytes());
        let mut out = [0u8; 32];
        for (index, word) in self.state.iter().enumerate() {
            out[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            w[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }

        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(text: &[u8]) -> String {
        let mut hasher = Sha256::new();
        hasher.update(text);
        hex(&hasher.finish())
    }

    #[test]
    fn sha256_matches_the_known_vectors() {
        assert_eq!(
            hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hash(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hash(&vec![b'a'; 1_000_000]),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn a_checksum_line_is_found_by_file_name() {
        let text = "abc123  other.zip\n\
                    e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  ./pokem-online-x86_64.AppImage\n";
        assert_eq!(
            parse_checksums(text, "pokem-online-x86_64.AppImage").as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
        assert!(parse_checksums(text, "missing.zip").is_none());
    }

    #[test]
    fn a_checksum_needs_the_full_hash() {
        assert!(parse_checksums("abc  pokem-online-x86_64.AppImage\n", "pokem-online-x86_64.AppImage").is_none());
    }

    #[test]
    fn installing_moves_the_old_copy_aside_and_keeps_it() {
        let dir = std::env::temp_dir().join(format!("pokem-update-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("launcher.AppImage");
        let staging = dir.join(".launcher.AppImage.update");
        let backup = dir.join(".launcher.AppImage.old");
        std::fs::write(&path, b"old").unwrap();
        std::fs::write(&staging, b"new").unwrap();

        let target = Target {
            kind: Kind::AppImage,
            path: path.clone(),
            part: dir.join(".launcher.AppImage.part"),
            staging,
            backup: backup.clone(),
            asset: "pokem-online-x86_64.AppImage",
            launch: path.clone(),
        };

        install(&target).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::read(&backup).unwrap(), b"old");
        assert!(!target.staging.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_install_puts_the_old_copy_back() {
        let dir = std::env::temp_dir().join(format!("pokem-update-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("launcher.AppImage");
        std::fs::write(&path, b"old").unwrap();

        let target = Target {
            kind: Kind::AppImage,
            path: path.clone(),
            part: dir.join(".part"),
            staging: dir.join("missing"),
            backup: dir.join(".old"),
            asset: "pokem-online-x86_64.AppImage",
            launch: path.clone(),
        };

        assert!(install(&target).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
