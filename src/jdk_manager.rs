use std::collections::HashMap;
use std::ffi::CStr;
use std::fs::{create_dir_all, File};
use std::io;
use std::io::{Read, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use indicatif::{MultiProgress, ProgressDrawTarget};
use log::debug;
use once_cell::sync::Lazy;
use tempdir::TempDir;

use crate::adoptjdk;
use crate::content_disposition_parser::parse_filename;
use crate::http_failure::handle_response_fail;
use crate::progress::new_progress_bar;

static BASE_PATH: Lazy<PathBuf> = Lazy::new(|| crate::config::PROJECT_DIRS.cache_dir().join("jdks"));
static BY_TTY: Lazy<PathBuf> = Lazy::new(|| std::env::temp_dir().join("jpre-by-tty"));

pub fn get_symlink_location() -> Result<PathBuf> {
    // Specifically check stderr, as stdout is likely to be redirected
    if !console::Term::stderr().features().is_attended() {
        return Err(anyhow!("Not a TTY"));
    }
    let tty = unsafe { CStr::from_ptr(libc::ttyname(libc::STDERR_FILENO)).to_str()? };
    let tty_as_name = tty.replace('/', "-");
    create_dir_all(&*BY_TTY).context("Failed to create by-tty directory")?;
    return Ok(BY_TTY.join(tty_as_name));
}

pub fn get_current_jdk() -> Result<String> {
    let symlink = get_symlink_location()?;
    let actual = symlink.read_link().context("No current JDK")?;
    return actual
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<u8>().ok())
        .and_then(|m| get_jdk_version(m))
        .context("Not linked to an actual JDK");
}

const FINISHED_MARKER: &str = ".jdk_marker";

pub fn get_jdk_version(major: u8) -> Option<String> {
    let path = BASE_PATH.join(major.to_string());
    if !path.join(FINISHED_MARKER).exists() {
        debug!("No finished marker exists in JDK {}", major);
        return None;
    }
    let release = path.join("release");
    if !path.join("release").exists() {
        debug!("No release file exists in JDK {}", major);
        return None;
    }
    let config = std::fs::read_to_string(release)
        .context("Failed to read release file")
        .and_then(|data| {
            toml::from_str::<HashMap<String, String>>(data.as_str())
                .context("Failed to parse TOML from release file")
        });
    match config {
        Ok(map) => map.get("JAVA_VERSION").map(|v| v.clone()),
        Err(error) => {
            debug!("{:?}", error);
            None
        }
    }
}

pub fn get_all_jdk_majors() -> Result<Vec<u8>> {
    let read_dir_result = BASE_PATH.read_dir();
    if let Err(read_dir_error) = read_dir_result {
        return if read_dir_error.kind() == std::io::ErrorKind::NotFound {
            // ignore if we can't find the dir
            Ok(Vec::new())
        } else {
            Err(read_dir_error)?
        };
    }
    return read_dir_result
        .context("Failed to read base directory")?
        .map(|res| {
            res.map(|e| {
                e.path()
                    .file_name()
                    // This should be impossible
                    .expect("cannot be missing file name")
                    .to_str()
                    // I don't really know if I should handle non-UTF-8
                    .expect("Non-UTF8 filename encountered")
                    .to_string()
            })
            .context("Failed to read directory entry")
        })
        .filter_map(|res| {
            match res {
                // map the parse error to None, otherwise get Some(Ok(u8))
                Ok(file_name) => file_name.parse::<u8>().ok().map(Ok),
                // map the actual errors back in
                Err(err) => Some(Err(err)),
            }
        })
        .collect();
}

pub fn map_available_jdk_versions(majors: &Vec<u8>) -> Vec<(u8, String)> {
    let mut vec: Vec<(u8, String)> = majors
        .iter()
        .filter_map(|jdk_major| get_jdk_version(*jdk_major).map(|version| (*jdk_major, version)))
        .collect();
    vec.sort_by_key(|v| v.0);
    return vec;
}

pub fn symlink_jdk_path(major: u8) -> Result<()> {
    let path = get_jdk_path(major).context("Failed to get JDK path")?;
    let symlink = get_symlink_location().context("Failed to get symlink location")?;
    if symlink.symlink_metadata().is_ok() {
        std::fs::remove_file(&symlink).context("Failed to remove old symlink")?;
    }
    std::os::unix::fs::symlink(path, symlink).context("Failed to make new symlink")?;
    Ok(())
}

pub fn get_jdk_path(major: u8) -> Result<PathBuf> {
    let path = BASE_PATH.join(major.to_string());
    if path.join(FINISHED_MARKER).exists() {
        return Ok(path);
    }

    update_jdk(major)?;
    return Ok(path);
}

pub fn update_jdk(major: u8) -> Result<()> {
    let path = BASE_PATH.join(major.to_string());
    let response = adoptjdk::get_latest_jdk_binary(major)?;
    if !response.is_success() {
        return Err(handle_response_fail(response, "Failed to get JDK binary"));
    }

    let url = response
        .headers()
        .get(attohttpc::header::CONTENT_DISPOSITION)
        .ok_or_else(|| anyhow!("no content disposition"))
        .and_then(|value| parse_filename(value.to_str()?))?;
    eprintln!("Extracting {}", url);
    if path.exists() {
        std::fs::remove_dir_all(&path)
            .with_context(|| format!("Unable to clean JDK folder ({})", path.display()))?;
    }
    create_dir_all(&path).with_context(|| {
        format!(
            "Unable to create directories to JDK folder ({})",
            path.display()
        )
    })?;
    let temporary_dir = TempDir::new_in(&*BASE_PATH, "jdk-download")
        .context("Failed to create temporary directory")?;
    finish_extract(&path, response, url, &temporary_dir).and_then(|_| {
        if temporary_dir.path().exists() {
            temporary_dir.close().context("Failed to cleanup temp dir")
        } else {
            Ok(())
        }
    })?;
    return Ok(());
}

fn finish_extract(
    path: &PathBuf,
    response: attohttpc::Response,
    url: String,
    temporary_dir: &TempDir,
) -> Result<()> {
    if url.ends_with(".tar.gz") {
        let expected_size = response.headers().get("Content-length").and_then(|len| {
            len.to_str()
                .ok()
                .and_then(|len_str| len_str.parse::<u64>().ok())
        });
        unarchive_tar_gz(temporary_dir.path(), expected_size, response)
    } else {
        return Err(anyhow!("Don't know how to handle {}", url));
    }
    eprintln!();
    let dir_entries = temporary_dir
        .path()
        .read_dir()
        .context("Failed to read temp dir")?
        .map(|res| res.map(|e| e.path()))
        .filter(|r| {
            match r {
                Ok(p) => match p.file_name() {
                    Some(name) => !name.to_string_lossy().starts_with("."),
                    _ => true,
                }
                _ => true,
            }
        })
        .collect::<Result<Vec<_>, io::Error>>()
        .context("Failed to read temp dir entry")?;
    let from_dir = if dir_entries.len() == 1 {
        if std::env::consts::OS == "macos" {
            let x = &dir_entries[0];
            x.join("Contents/Home")
        } else {
            (&dir_entries[0]).to_path_buf()
        }
    } else {
        temporary_dir.path().to_path_buf()
    };

    std::fs::rename(from_dir, &path)
        .with_context(|| format!("Unable to move to JDK folder ({})", path.display()))?;

    File::create(path.join(FINISHED_MARKER)).context("Unable to create marker")?;
    Ok(())
}

fn unarchive_tar_gz(path: &Path, expected_size: Option<u64>, reader: impl Read + Send + 'static) {
    let all_bars = MultiProgress::with_draw_target(ProgressDrawTarget::stderr());
    let download_bar = all_bars.add(new_progress_bar(expected_size));
    download_bar.set_message("Download progress");
    let writing_bar = all_bars.add(new_progress_bar(None));

    let static_path = path.to_path_buf();
    let _ = std::thread::spawn(move || {
        let gz_decode = libflate::gzip::Decoder::new(BufReader::new(download_bar.wrap_read(reader))).unwrap();
        let mut archive = tar::Archive::new(BufReader::new(writing_bar.wrap_read(gz_decode)));
        archive.set_preserve_permissions(true);
        archive.set_overwrite(true);
        for entry in archive.entries().unwrap() {
            let mut file = entry.unwrap();
            writing_bar.set_message(&*format!("Extracting {}", file.path().unwrap().display()));
            file.unpack_in(&static_path).unwrap();
        }
        download_bar.finish();
        writing_bar.abandon_with_message("Done extracting!");
    });

    all_bars.join().unwrap();
}
