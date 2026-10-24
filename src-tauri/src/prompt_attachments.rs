//! One-off inputs the user adds to a message: images normalized into the PNG
//! attachment pipeline every harness accepts, documents reduced to bounded text,
//! and folders summarized as a listing. Paths arrive from native pickers, drops
//! and pastes in the trusted main webview; agents never address them directly.
use crate::workspace;
use crate::workspace::run_process;
use base64::{engine::general_purpose::STANDARD, Engine};
use image::{codecs::jpeg::JpegEncoder, DynamicImage, ImageFormat, ImageReader, Limits};
use serde::Serialize;
use std::{
    fs::File,
    io::{Cursor, Read},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const MAX_IMAGE_SOURCE: usize = 24 * 1024 * 1024;
/// What `providers::attachments::load` accepts per image.
const MAX_IMAGE_PNG: usize = 5 * 1024 * 1024;
const MAX_IMAGE_EDGE: u32 = 2048;
/// Shared with project-file context blocks, so one message stays bounded.
pub const MAX_TEXT: usize = 32 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 64 * 1024 * 1024;
const EXTRACTOR_OUTPUT: usize = 12 * 1024 * 1024;
const EXTRACTOR_TIMEOUT: Duration = Duration::from_secs(30);
/// One part of an Office/EPUB container, and the whole of it, before the text is
/// cut down to `MAX_TEXT`: enough for a real document, nothing like a zip bomb.
const MAX_CONTAINER_PART: u64 = 4 * 1024 * 1024;
const MAX_CONTAINER_TEXT: usize = 16 * 1024 * 1024;
const MAX_CONTAINER_PARTS: usize = 400;
const LISTING_LINES: usize = 240;
const LISTING_DEPTH: usize = 3;
const LISTING_ENTRIES: usize = 20_000;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportedImage {
    pub id: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    /// Small JPEG data URL for the composer chip; never sent to a model.
    pub thumbnail: String,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentText {
    pub name: String,
    pub path: String,
    pub size: u64,
    pub text: String,
    pub truncated: bool,
    pub method: &'static str,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderSummary {
    pub name: String,
    pub path: String,
    pub files: usize,
    pub folders: usize,
    pub listing: String,
    pub truncated: bool,
}

pub enum ImageSource {
    Path(PathBuf),
    Data { name: String, data_url: String },
}

fn file_name(path: &Path, fallback: &str) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

fn checked_file(path: &Path, max: u64, what: &str) -> Result<(File, u64), String> {
    if !path.is_absolute() {
        return Err(format!("Choose the {what} with the picker or drop it onto the message."));
    }
    let file = File::open(path).map_err(|_| format!("Could not open this {what}."))?;
    let metadata = file
        .metadata()
        .map_err(|_| format!("Could not read this {what}."))?;
    if !metadata.is_file() {
        return Err(format!("This is not a {what} file."));
    }
    if metadata.len() > max {
        return Err(format!("This {what} is larger than {} MB.", max / 1024 / 1024));
    }
    Ok((file, metadata.len()))
}

fn read_bounded(file: File, max: usize) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > max {
        return Err(format!("The file is larger than {} MB.", max / 1024 / 1024));
    }
    Ok(bytes)
}

fn decode(bytes: &[u8]) -> Result<(DynamicImage, ImageFormat), String> {
    let format = image::guess_format(bytes)
        .map_err(|_| "Choose a PNG, JPEG or WebP image. Screenshots and exports work well.")?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP
    ) {
        return Err("Choose a PNG, JPEG or WebP image. HEIC, GIF and SVG are not supported yet.".into());
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(12_000);
    limits.max_image_height = Some(12_000);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|_| "This image is damaged or too large to decode.".to_string())?;
    Ok((image, format))
}

fn encode_png(image: &DynamicImage, edge: u32) -> Result<Vec<u8>, String> {
    let scaled = if image.width() > edge || image.height() > edge {
        image.thumbnail(edge, edge)
    } else {
        image.clone()
    };
    let mut bytes = Vec::new();
    scaled
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .map_err(|_| "Could not prepare this image.".to_string())?;
    Ok(bytes)
}

fn thumbnail(image: &DynamicImage) -> Result<String, String> {
    let small = image.thumbnail(320, 320).to_rgba8();
    // Composite onto white so transparent sketches stay readable in the chip.
    let rgb = image::RgbImage::from_fn(small.width(), small.height(), |x, y| {
        let p = small.get_pixel(x, y).0;
        let a = p[3] as u32;
        image::Rgb([
            ((p[0] as u32 * a + 255 * (255 - a)) / 255) as u8,
            ((p[1] as u32 * a + 255 * (255 - a)) / 255) as u8,
            ((p[2] as u32 * a + 255 * (255 - a)) / 255) as u8,
        ])
    });
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, 72)
        .encode_image(&rgb)
        .map_err(|_| "Could not prepare this image.".to_string())?;
    Ok(format!("data:image/jpeg;base64,{}", STANDARD.encode(bytes)))
}

/// Normalize any supported image into a PNG the attachment pipeline accepts and
/// store it beside browser screenshots, which share their IDs, limits and cleanup.
pub fn import_image(dir: &Path, source: ImageSource) -> Result<ImportedImage, String> {
    let (name, bytes) = match source {
        ImageSource::Path(path) => {
            let (file, _) = checked_file(&path, MAX_IMAGE_SOURCE as u64, "image")?;
            (file_name(&path, "Image.png"), read_bounded(file, MAX_IMAGE_SOURCE)?)
        }
        ImageSource::Data { name, data_url } => {
            if data_url.len() > MAX_IMAGE_SOURCE * 4 / 3 + 128 {
                return Err("Each image must be 24 MB or smaller.".into());
            }
            let (prefix, data) = data_url.split_once(',').ok_or("Invalid image data.")?;
            if !matches!(
                prefix,
                "data:image/png;base64" | "data:image/jpeg;base64" | "data:image/webp;base64"
            ) {
                return Err("Choose a PNG, JPEG or WebP image.".into());
            }
            let bytes = STANDARD.decode(data).map_err(|_| "Invalid image data.")?;
            let name = name.trim();
            let name = if name.is_empty() || name.len() > 200 || name.chars().any(char::is_control) {
                "Image.png".to_owned()
            } else {
                name.to_owned()
            };
            (name, bytes)
        }
    };
    let (image, format) = decode(&bytes)?;
    let (width, height) = (image.width(), image.height());
    let png = if format == ImageFormat::Png
        && bytes.len() <= MAX_IMAGE_PNG
        && width.max(height) <= MAX_IMAGE_EDGE
    {
        bytes
    } else {
        let mut png = encode_png(&image, MAX_IMAGE_EDGE)?;
        for edge in [1600, 1200] {
            if png.len() <= MAX_IMAGE_PNG {
                break;
            }
            png = encode_png(&image, edge)?;
        }
        if png.len() > MAX_IMAGE_PNG {
            return Err("This image is too detailed to attach. Export it smaller and try again.".into());
        }
        png
    };
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let path = dir.join(format!("{id}.png"));
    std::fs::write(&path, &png).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(ImportedImage {
        id,
        name,
        width,
        height,
        thumbnail: thumbnail(&image)?,
    })
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default()
}

/// A client's filename as a person reads it: no control characters, bounded, and
/// never empty. It is display text only — the bytes are read from the path.
fn display_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "Document".to_owned()
    } else {
        cleaned.to_owned()
    }
}

/// Decode the string literal that `mdimport -d3` prints for `kMDItemTextContent`:
/// C-style escapes plus `\Uxxxx` code units (surrogate pairs for astral characters).
pub(crate) fn spotlight_text(dump: &str) -> Option<String> {
    let marker = "kMDItemTextContent = \"";
    let start = dump.find(marker)? + marker.len();
    let mut chars = dump[start..].chars();
    let mut text = String::new();
    let mut high: Option<u16> = None;
    loop {
        // Running out of input before the closing quote means the dump was cut short.
        let (unit, ch) = match chars.next()? {
            '"' => break,
            '\\' => match chars.next()? {
                'n' => (None, Some('\n')),
                't' => (None, Some('\t')),
                'r' => (None, Some('\r')),
                'U' | 'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    match u16::from_str_radix(&hex, 16) {
                        Ok(unit) => (Some(unit), None),
                        Err(_) => (None, Some('\u{FFFD}')),
                    }
                }
                other => (None, Some(other)),
            },
            other => (None, Some(other)),
        };
        // UTF-16 units pair up; anything else flushes a dangling high surrogate.
        match (unit, ch) {
            (Some(unit @ 0xDC00..=0xDFFF), _) if high.is_some() => {
                let lead = high.take().unwrap() as u32;
                let code = 0x10000 + ((lead - 0xD800) << 10) + (unit as u32 - 0xDC00);
                text.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
            }
            (Some(unit), _) => {
                if high.take().is_some() {
                    text.push('\u{FFFD}');
                }
                if (0xD800..=0xDBFF).contains(&unit) {
                    high = Some(unit);
                } else {
                    text.push(char::from_u32(unit as u32).unwrap_or('\u{FFFD}'));
                }
            }
            (None, Some(ch)) => {
                if high.take().is_some() {
                    text.push('\u{FFFD}');
                }
                text.push(ch);
            }
            (None, None) => {}
        }
    }
    Some(text)
}

fn truncate_text(mut text: String, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

fn looks_like_text(bytes: &[u8]) -> Option<String> {
    if bytes.contains(&0) {
        return None;
    }
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text.to_owned(),
        // A read that stopped mid-character is still text; anything else is binary.
        Err(error) if error.error_len().is_none() => {
            String::from_utf8_lossy(&bytes[..error.valid_up_to()]).into_owned()
        }
        Err(_) => return None,
    };
    let control = text
        .chars()
        .filter(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{0C}'))
        .count();
    (control * 100 <= text.chars().count().max(1)).then_some(text)
}

#[cfg(target_os = "macos")]
fn extract_with(tool: &str, args: &[&str], path: &Path) -> Result<String, String> {
    let mut command = Command::new(tool);
    command.args(args).arg(path);
    let output = run_process(command, tool, EXTRACTOR_OUTPUT, EXTRACTOR_TIMEOUT)?;
    if !output.status.success() && output.stdout.bytes.is_empty() {
        return Err(format!(
            "Could not read this document with {tool}: {}",
            String::from_utf8_lossy(&output.stderr.bytes).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout.bytes).into_owned())
}

/// No platform reader here: this is the answer for a format this host cannot read,
/// and the sentence says which host can.
#[cfg(not(target_os = "macos"))]
fn extract_with(_tool: &str, _args: &[&str], _path: &Path) -> Result<String, String> {
    Err(UNREADABLE_HERE.into())
}

/// Kept verbatim from the macOS-only era: it is the answer for the formats this
/// host still cannot read (`doc`, `rtf`, `pages`, `pptx`, `xlsx`, an unknown
/// binary), and it is true of them — macOS reads them through `textutil` and
/// Spotlight, and a text export is what a Linux reader should attach instead.
#[cfg(not(target_os = "macos"))]
const UNREADABLE_HERE: &str =
    "Reading PDF and Office documents is available on macOS. Attach a text export instead.";

/// The external readers this host has.
///
/// A value, not a global: `read_document_as` hands over the host's own answer, and a
/// test hands over a host with a package missing, which is the only way to prove the
/// message that names it without removing anything from a machine.
struct Tools {
    /// `pdftotext` from poppler-utils, the PDF reader off macOS. Only that path
    /// looks a tool up, so a macOS build has nothing to warn about here.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pdftotext: Option<PathBuf>,
}

impl Tools {
    fn host() -> Self {
        Self {
            pdftotext: find_in_path("pdftotext"),
        }
    }
}

/// A tool looked up the way a shell would: the first `PATH` entry holding a file of
/// that name. An absolute path to the tool is then used, so what was checked is what
/// runs.
fn find_in_path(tool: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(tool))
        .find(|candidate| candidate.is_file())
}

/// A PDF's text layer through poppler, inside the same 30 s / 12 MiB extractor
/// budget the macOS tools get: one bound for every external reader.
#[cfg(not(target_os = "macos"))]
fn pdf_text(path: &Path, tools: &Tools) -> Result<String, String> {
    let tool = tools.pdftotext.as_ref().ok_or_else(|| {
        "Reading PDFs on this machine needs poppler-utils, which is not installed. Install poppler-utils and try again."
            .to_string()
    })?;
    let mut command = Command::new(tool);
    // `-q` keeps poppler's own notices out of the text; `-` writes it to stdout.
    command.args(["-q", "-enc", "UTF-8"]).arg(path).arg("-");
    let output = run_process(command, "pdftotext", EXTRACTOR_OUTPUT, EXTRACTOR_TIMEOUT)?;
    if !output.status.success() && output.stdout.bytes.is_empty() {
        let detail = String::from_utf8_lossy(&output.stderr.bytes).trim().to_owned();
        return Err(if detail.is_empty() {
            "This PDF could not be read. It may be damaged.".to_owned()
        } else {
            format!("Could not read this PDF: {detail}")
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout.bytes).into_owned())
}

/// The text of an Office or EPUB container through the `zip` crate, which is
/// already a dependency for VS Code themes. No extractor binary is involved, so the
/// only "missing" case is a file that is not the container it claims to be.
#[cfg(not(target_os = "macos"))]
fn container_text(file: File, ext: &str) -> Result<String, String> {
    let mut archive = zip::ZipArchive::new(file).map_err(|_| {
        format!("This {ext} file could not be opened as a document. It may be damaged or misnamed.")
    })?;
    let parts = match ext {
        "docx" => vec!["word/document.xml".to_owned()],
        "odt" => vec!["content.xml".to_owned()],
        _ => epub_parts(&mut archive),
    };
    let mut text = String::new();
    for part in parts {
        if text.len() >= MAX_CONTAINER_TEXT {
            break;
        }
        if let Some(part_text) = container_part(&mut archive, &part)? {
            text.push_str(&part_text);
            text.push_str("\n\n");
        }
    }
    if text.trim().is_empty() {
        return Err(format!(
            "This {ext} file has no readable document text inside it."
        ));
    }
    Ok(text)
}

/// One entry of a container, read with a bound and reduced to text. `None` when the
/// entry is absent, so a caller can tell "no such part" from "empty part".
#[cfg(not(target_os = "macos"))]
fn container_part(
    archive: &mut zip::ZipArchive<File>,
    name: &str,
) -> Result<Option<String>, String> {
    Ok(container_raw(archive, name)?.map(|raw| {
        tidy_text(&decode_entities(&xml_text(&raw)))
    }))
}

/// The same entry before it becomes text: the EPUB spine and manifest have to be
/// read as markup, so the tags are still here.
#[cfg(not(target_os = "macos"))]
fn container_raw(archive: &mut zip::ZipArchive<File>, name: &str) -> Result<Option<String>, String> {
    let Ok(entry) = archive.by_name(name) else {
        return Ok(None);
    };
    let mut bytes = Vec::new();
    entry
        .take(MAX_CONTAINER_PART)
        .read_to_end(&mut bytes)
        .map_err(|_| format!("Could not read {name} out of this document."))?;
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// The reading order of an EPUB: `META-INF/container.xml` names the package
/// document, whose spine lists the chapters in order. A container whose spine
/// cannot be read falls back to every HTML part in natural name order, which is a
/// worse order but still the book's text.
#[cfg(not(target_os = "macos"))]
fn epub_parts(archive: &mut zip::ZipArchive<File>) -> Vec<String> {
    if let Some(parts) = epub_spine(archive) {
        if !parts.is_empty() {
            return parts;
        }
    }
    let mut names: Vec<String> = archive.file_names().map(str::to_owned).collect();
    names.sort_by(|a, b| natural_order(a, b));
    names
        .into_iter()
        .filter(|name| {
            let lower = name.to_ascii_lowercase();
            lower.ends_with(".xhtml") || lower.ends_with(".html") || lower.ends_with(".htm")
        })
        .take(MAX_CONTAINER_PARTS)
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn epub_spine(archive: &mut zip::ZipArchive<File>) -> Option<Vec<String>> {
    let container = container_raw(archive, "META-INF/container.xml").ok()??;
    let opf = elements(&container, "rootfile")
        .first()
        .and_then(|element| attribute(element, "full-path"))?;
    let package = container_raw(archive, &opf).ok()??;
    // Hrefs are relative to the package document's own directory.
    let base = opf.rsplit_once('/').map(|(dir, _)| dir.to_owned());
    let items: Vec<(String, String)> = elements(&package, "item")
        .into_iter()
        .filter_map(|item| {
            Some((
                attribute(item, "id")?,
                attribute(item, "href")?.replace('\\', "/"),
            ))
        })
        .collect();
    let mut parts = Vec::new();
    for reference in elements(&package, "itemref") {
        let Some(idref) = attribute(reference, "idref") else {
            continue;
        };
        let Some((_, href)) = items.iter().find(|(id, _)| *id == idref) else {
            continue;
        };
        let path = match (&base, href.contains("://")) {
            (_, true) => continue,
            (Some(dir), _) => format!("{dir}/{href}"),
            (None, _) => href.clone(),
        };
        // A query or fragment is not part of the entry name.
        let path = path.split(['?', '#']).next().unwrap_or(&path);
        parts.push(percent_decode(path));
        if parts.len() >= MAX_CONTAINER_PARTS {
            break;
        }
    }
    Some(parts)
}

/// The elements of one name in an XML document, as their own text (attributes and
/// children included up to the next tag). Enough for the few attributes this module
/// needs, and it parses nothing it is not asked for.
#[cfg(not(target_os = "macos"))]
fn elements<'a>(xml: &'a str, want: &str) -> Vec<&'a str> {
    xml.split('<')
        .filter(|chunk| {
            chunk.strip_prefix(want).is_some_and(|rest| {
                rest.starts_with(' ') || rest.starts_with('\t') || rest.starts_with('>') || rest.starts_with('/')
            })
        })
        .collect()
}

/// One attribute's value, quoted with either kind of quote.
#[cfg(not(target_os = "macos"))]
fn attribute(element: &str, name: &str) -> Option<String> {
    let start = element.find(name)?;
    let rest = element[start + name.len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let value = &rest[1..];
    let end = value.find(quote)?;
    Some(value[..end].to_owned())
}

/// Case-insensitive natural order, so chapter2 sorts before chapter10.
#[cfg(not(target_os = "macos"))]
fn natural_order(a: &str, b: &str) -> std::cmp::Ordering {
    let mut left = a.chars().peekable();
    let mut right = b.chars().peekable();
    loop {
        match (left.peek().copied(), right.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(one), Some(other)) if one.is_ascii_digit() && other.is_ascii_digit() => {
                let mut first = String::new();
                while let Some(c) = left.peek().copied().filter(char::is_ascii_digit) {
                    first.push(c);
                    left.next();
                }
                let mut second = String::new();
                while let Some(c) = right.peek().copied().filter(char::is_ascii_digit) {
                    second.push(c);
                    right.next();
                }
                let first = first.trim_start_matches('0');
                let second = second.trim_start_matches('0');
                let order = first
                    .len()
                    .cmp(&second.len())
                    .then_with(|| first.cmp(second));
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
            }
            (Some(one), Some(other)) => {
                let order = one.to_ascii_lowercase().cmp(&other.to_ascii_lowercase());
                if order != std::cmp::Ordering::Equal {
                    return order;
                }
                left.next();
                right.next();
            }
        }
    }
}

/// The text of one XML/HTML part: tags dropped, block boundaries turned into line
/// breaks, and the entities a document actually uses decoded. There is no DTD here
/// and no external anything, so entity expansion cannot be a way into this process.
#[cfg(not(target_os = "macos"))]
fn xml_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len() / 2);
    let mut characters = input.char_indices().peekable();
    while let Some((index, first)) = characters.next() {
        if first != '<' {
            out.push(first);
            continue;
        }
        let start = index + 1;
        let Some(end) = input[start..].find('>').map(|end| start + end) else {
            // An unterminated tag is not markup we can read: drop what is left.
            break;
        };
        let tag = input[start..end].trim();
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or_default();
        let local = name.rsplit(':').next().unwrap_or(name).to_ascii_lowercase();
        match local.as_str() {
            "tab" => out.push('\t'),
            "p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "tr" | "li" | "div" | "br"
            | "line-break" | "title" => out.push('\n'),
            _ => {}
        }
        // Skip the rest of this tag; the iterator stopped just after '<'.
        while characters.peek().is_some_and(|(next, _)| *next <= end) {
            characters.next();
        }
    }
    out
}

/// The five XML entities, the handful of named ones documents use, and numeric
/// references. An entity this does not know stays as written rather than becoming
/// invented text.
#[cfg(not(target_os = "macos"))]
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(end) = after.find(';').filter(|end| *end <= 12) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let decoded = match &after[1..end] {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "hellip" => Some('…'),
            "lsquo" | "rsquo" => Some('\''),
            "ldquo" | "rdquo" => Some('"'),
            entity => entity.strip_prefix('#').and_then(|number| {
                let code = match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => number.parse().ok()?,
                };
                char::from_u32(code).filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
            }),
        };
        match decoded {
            Some(character) => out.push(character),
            None => out.push_str(&after[..=end]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Lines with trailing space removed and runs of blank lines collapsed, so the text
/// a model reads is not mostly indentation from the source markup.
#[cfg(not(target_os = "macos"))]
fn tidy_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let normalised = text.replace("\r\n", "\n").replace('\r', "\n");
    for line in normalised.split('\n') {
        let line = line.trim_end();
        if line.is_empty() && (out.is_empty() || out.ends_with("\n\n")) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim().to_owned()
}

/// `%XX` in an EPUB href, which is a URL, not a file name.
#[cfg(not(target_os = "macos"))]
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).ok();
            if let Some(byte) = hex.and_then(|hex| u8::from_str_radix(hex, 16).ok()) {
                decoded.push(byte);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

/// Reduce a document to bounded text. Plain text is read directly; Office and
/// rich-text formats go through `textutil`, everything else through Spotlight's
/// importers (`mdimport`), which cover PDF, iWork, EPUB and many more.
///
/// Off macOS the readers are poppler's `pdftotext` for PDF and the `zip` crate for
/// the DOCX/ODT/EPUB containers; everything else keeps the honest sentence.
pub fn read_document(path: &Path) -> Result<DocumentText, String> {
    let name = file_name(path, "Document");
    read_document_as(path, &name)
}

/// The same, for a file whose stored path says nothing about its format: an upload
/// is `<uuid>/data`, so the format has to come from the name the client gave it.
pub(crate) fn read_document_as(path: &Path, name: &str) -> Result<DocumentText, String> {
    read_document_with(path, name, &Tools::host())
}

fn read_document_with(
    path: &Path,
    name: &str,
    #[cfg_attr(target_os = "macos", allow(unused_variables))] tools: &Tools,
) -> Result<DocumentText, String> {
    let (mut file, size) = checked_file(path, MAX_DOCUMENT_BYTES, "document")?;
    let name = display_name(name);
    let ext = extension(&name);
    let mut cut = false;
    let (text, method) = match ext.as_str() {
        // macOS keeps its own readers, unchanged.
        #[cfg(target_os = "macos")]
        "doc" | "docx" | "rtf" | "rtfd" | "odt" | "wordml" | "webarchive" => (
            extract_with("/usr/bin/textutil", &["-convert", "txt", "-stdout"], path)?,
            "textutil",
        ),
        #[cfg(target_os = "macos")]
        "pdf" | "pages" | "key" | "numbers" | "pptx" | "ppt" | "xlsx" | "xls" | "epub" => {
            (spotlight(path)?, "spotlight")
        }
        // Off macOS: poppler for PDF, the zip crate for the three containers.
        #[cfg(not(target_os = "macos"))]
        "pdf" => (pdf_text(path, tools)?, "pdftotext"),
        #[cfg(not(target_os = "macos"))]
        "docx" | "odt" | "epub" => (container_text(file, &ext)?, "zip"),
        _ => {
            // Only the head is read: a text file beyond the budget is cut, not rejected.
            let mut head = Vec::new();
            (&mut file)
                .take(MAX_TEXT as u64 + 1)
                .read_to_end(&mut head)
                .map_err(|_| "Could not read this document.".to_string())?;
            cut = head.len() > MAX_TEXT;
            head.truncate(MAX_TEXT);
            match looks_like_text(&head) {
                Some(text) => (text, "text"),
                None => (spotlight(path)?, "spotlight"),
            }
        }
    };
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err("No readable text was found in this document. It may be scanned images only.".into());
    }
    let (text, truncated) = truncate_text(text, MAX_TEXT);
    Ok(DocumentText {
        name,
        path: path.to_string_lossy().into_owned(),
        size,
        text,
        truncated: truncated || cut,
        method,
    })
}

fn spotlight(path: &Path) -> Result<String, String> {
    let dump = extract_with("/usr/bin/mdimport", &["-t", "-d3"], path)?;
    spotlight_text(&dump).ok_or_else(|| {
        "No readable text was found in this document. It may be scanned images only.".into()
    })
}

fn hidden(name: &str) -> bool {
    name.starts_with('.')
}

/// A shallow, sorted tree of a folder: enough structure for a model to orient
/// itself, never a full walk. Build folders and version control are skipped.
pub fn describe_folder(path: &Path) -> Result<FolderSummary, String> {
    if !path.is_absolute() {
        return Err("Choose the folder with the picker or drop it onto the message.".into());
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|_| "Could not open this folder.")?;
    if !metadata.is_dir() {
        return Err("This is not a folder.".into());
    }
    let name = file_name(path, &path.to_string_lossy());
    let mut listing = Listing {
        lines: Vec::new(),
        files: 0,
        folders: 0,
        budget: LISTING_ENTRIES,
        truncated: false,
        deadline: Instant::now() + Duration::from_secs(2),
    };
    listing.walk(path, 0);
    Ok(FolderSummary {
        name,
        path: path.to_string_lossy().into_owned(),
        files: listing.files,
        folders: listing.folders,
        listing: listing.lines.join("\n"),
        truncated: listing.truncated,
    })
}

struct Listing {
    lines: Vec<String>,
    files: usize,
    folders: usize,
    budget: usize,
    truncated: bool,
    deadline: Instant,
}

impl Listing {
    fn walk(&mut self, dir: &Path, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<(String, bool)> = entries
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let kind = entry.file_type().ok()?;
                let name = entry.file_name().to_str()?.to_owned();
                if kind.is_symlink()
                    || hidden(&name)
                    || workspace::excluded_entry(&name, kind.is_dir())
                {
                    return None;
                }
                (kind.is_dir() || kind.is_file()).then_some((name, kind.is_dir()))
            })
            .collect();
        entries.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
        });
        let indent = "  ".repeat(depth);
        for (index, (name, is_dir)) in entries.iter().enumerate() {
            if self.budget == 0 || Instant::now() >= self.deadline {
                self.truncated = true;
                return;
            }
            self.budget -= 1;
            if *is_dir {
                self.folders += 1;
            } else {
                self.files += 1;
            }
            if self.lines.len() >= LISTING_LINES {
                self.truncated = true;
                continue;
            }
            let remaining = entries.len() - index;
            if self.lines.len() + 1 == LISTING_LINES && remaining > 1 {
                self.lines.push(format!("{indent}… {remaining} more"));
                self.truncated = true;
                continue;
            }
            self.lines
                .push(format!("{indent}{name}{}", if *is_dir { "/" } else { "" }));
            if *is_dir && depth + 1 < LISTING_DEPTH {
                self.walk(&dir.join(name), depth + 1);
            } else if *is_dir {
                self.truncated = true;
            }
        }
    }
}

#[cfg(test)]
/// A one-page PDF whose content stream is `content`, with a correct xref table so
/// poppler reads it the way it reads a real file. Test support: the extractor tests
/// in `uploads` use it too.
#[cfg(test)]
pub(crate) fn sample_pdf(content: &str) -> Vec<u8> {
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_string(),
        format!("<< /Length {} >>\nstream\n{content}\nendstream", content.len()),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (index, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{}\nendobj\n", index + 1, object));
    }
    let xref = pdf.len();
    pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1));
    for offset in &offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    ));
    pdf.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pandacode-{label}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn imports_png_data_and_files_as_png_attachments_with_thumbnails() {
        let dir = temp_dir("images");
        let imported = import_image(
            &dir.join("captures"),
            ImageSource::Data { name: "Mockup.png".into(), data_url: format!("data:image/png;base64,{PNG}") },
        )
        .unwrap();
        assert_eq!((imported.width, imported.height, imported.name.as_str()), (1, 1, "Mockup.png"));
        assert!(imported.thumbnail.starts_with("data:image/jpeg;base64,"));
        let stored = dir.join("captures").join(format!("{}.png", imported.id));
        assert!(std::fs::read(&stored).unwrap().starts_with(b"\x89PNG"));
        uuid::Uuid::parse_str(&imported.id).unwrap();

        // A JPEG on disk is re-encoded as PNG so every harness receives the same format.
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 80)
            .encode_image(&image::RgbImage::from_pixel(3000, 40, image::Rgb([200, 30, 30])))
            .unwrap();
        let photo = dir.join("photo.jpg");
        std::fs::write(&photo, jpeg).unwrap();
        let imported = import_image(&dir.join("captures"), ImageSource::Path(photo)).unwrap();
        assert_eq!((imported.width, imported.height), (3000, 40));
        let bytes = std::fs::read(dir.join("captures").join(format!("{}.png", imported.id))).unwrap();
        let (decoded, format) = decode(&bytes).unwrap();
        assert_eq!(format, ImageFormat::Png);
        assert!(decoded.width() <= MAX_IMAGE_EDGE, "large sources are scaled to the pipeline's edge");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_non_images_relative_paths_and_foreign_data_urls() {
        let dir = temp_dir("bad-images");
        assert!(import_image(&dir, ImageSource::Path(PathBuf::from("relative.png"))).is_err());
        let text = dir.join("notes.png");
        std::fs::write(&text, b"not an image").unwrap();
        assert!(import_image(&dir, ImageSource::Path(text)).unwrap_err().contains("PNG, JPEG or WebP"));
        assert!(import_image(&dir, ImageSource::Data { name: "x".into(), data_url: "data:image/svg+xml;base64,PHN2Zy8+".into() }).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn decodes_spotlight_string_literals() {
        let dump = "{\n    kMDItemContentType = \"com.adobe.pdf\";\n    kMDItemTextContent = \"Hello \\\"quoted\\\" caf\\U00e9 \\U2014 \\U2713\\nTab\\there \\\\ done \\Ud83d\\Ude00!\";\n    kMDItemTitle = \"x\";\n}";
        assert_eq!(spotlight_text(dump).unwrap(), "Hello \"quoted\" café — ✓\nTab\there \\ done 😀!");
        assert_eq!(spotlight_text("{ kMDItemTitle = \"x\"; }"), None);
        assert_eq!(spotlight_text("kMDItemTextContent = \"unterminated"), None);
    }

    #[test]
    fn reads_plain_text_documents_bounded_and_refuses_binary_without_an_extractor() {
        let dir = temp_dir("docs");
        let notes = dir.join("notes.md");
        std::fs::write(&notes, "# Plan\n\nShip it — with unicode ✓\n").unwrap();
        let document = read_document(&notes).unwrap();
        assert_eq!((document.method, document.truncated), ("text", false));
        assert_eq!(document.text, "# Plan\n\nShip it — with unicode ✓");
        assert_eq!(document.name, "notes.md");

        let big = dir.join("big.log");
        std::fs::write(&big, "x".repeat(MAX_TEXT + 500)).unwrap();
        let document = read_document(&big).unwrap();
        assert!(document.truncated);
        assert_eq!(document.text.len(), MAX_TEXT);

        assert!(read_document(Path::new("relative.txt")).is_err());
        assert!(read_document(&dir).is_err(), "folders are not documents");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extracts_text_from_rich_text_and_spotlight_indexed_documents() {
        let dir = temp_dir("rich");
        let rtf = dir.join("brief.rtf");
        std::fs::write(&rtf, "{\\rtf1\\ansi Bold {\\b brief} text.}").unwrap();
        let document = read_document(&rtf).unwrap();
        assert_eq!(document.method, "textutil");
        assert!(document.text.contains("Bold brief text."), "{}", document.text);

        // A minimal PDF goes through Spotlight's PDF importer.
        let pdf = dir.join("brief.pdf");
        std::fs::write(&pdf, b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 300 144] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >> endobj\n4 0 obj << /Length 44 >> stream\nBT /F1 24 Tf 20 60 Td (Hello PandaCode) Tj ET\nendstream endobj\n5 0 obj << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> endobj\ntrailer << /Root 1 0 R >>\n%%EOF\n").unwrap();
        let document = read_document(&pdf).unwrap();
        assert_eq!(document.method, "spotlight");
        assert!(document.text.contains("Hello PandaCode"), "{}", document.text);

        // Binary data with no importer is refused rather than pasted as garbage.
        let blob = dir.join("page.unknownext");
        std::fs::write(&blob, b"\x00\x01binary prefix").unwrap();
        let error = read_document(&blob).unwrap_err();
        assert!(error.contains("No readable text") || error.contains("Could not read"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn describes_folders_as_a_shallow_sorted_tree_skipping_build_output() {
        let dir = temp_dir("folder");
        std::fs::create_dir_all(dir.join("src/components/deep/deeper")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("README.md"), "").unwrap();
        std::fs::write(dir.join("src/main.ts"), "").unwrap();
        std::fs::write(dir.join("src/components/Button.tsx"), "").unwrap();
        std::fs::write(dir.join("src/components/deep/x.ts"), "").unwrap();
        std::fs::write(dir.join(".hidden"), "").unwrap();
        let summary = describe_folder(&dir).unwrap();
        assert_eq!(
            summary.listing,
            "src/\n  components/\n    deep/\n    Button.tsx\n  main.ts\nREADME.md"
        );
        assert_eq!((summary.files, summary.folders), (3, 3));
        assert!(summary.truncated, "a folder below the depth limit marks the listing partial");
        assert!(!summary.listing.contains("node_modules") && !summary.listing.contains(".git"));
        assert!(describe_folder(&dir.join("README.md")).is_err());
        assert!(describe_folder(Path::new("relative")).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn listing_stops_at_the_line_budget_with_a_remainder_note() {
        let dir = temp_dir("wide");
        for index in 0..(LISTING_LINES + 20) {
            std::fs::write(dir.join(format!("file-{index:04}.txt")), "").unwrap();
        }
        let summary = describe_folder(&dir).unwrap();
        assert!(summary.truncated);
        assert_eq!(summary.listing.lines().count(), LISTING_LINES);
        assert!(summary.listing.lines().last().unwrap().starts_with("… 21 more"));
        assert_eq!(summary.files, LISTING_LINES + 20, "counts continue past the printed lines");
        let _ = std::fs::remove_dir_all(dir);
    }

    // ---- off-macOS readers (PAN-109) ---------------------------------------

    /// The host's own tools, or the absence of one, as a value.
    #[cfg(not(target_os = "macos"))]
    fn with_pdftotext(path: Option<PathBuf>) -> Tools {
        Tools { pdftotext: path }
    }

    #[cfg(not(target_os = "macos"))]
    fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    /// A container with the given entries, written the way a real Office file is.
    #[cfg(not(target_os = "macos"))]
    fn container(parts: &[(&str, &str)]) -> Vec<u8> {
        use std::io::Write;
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options = zip::write::SimpleFileOptions::default();
            for (name, contents) in parts {
                writer.start_file(*name, options).unwrap();
                writer.write_all(contents.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_pdf_yields_the_words_on_its_page_through_pdftotext() {
        let dir = temp_dir("pdf-text");
        let path = write(
            &dir,
            "brief.pdf",
            &sample_pdf("BT /F1 18 Tf 72 700 Td (Hello PandaCode upload) Tj ET"),
        );
        let document = read_document_as(&path, "brief.pdf").unwrap();
        assert_eq!(document.method, "pdftotext");
        assert!(
            document.text.contains("Hello PandaCode upload"),
            "{}",
            document.text
        );
        assert!(!document.truncated);
        assert!(document.text.len() <= MAX_TEXT);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_pdf_without_a_text_layer_says_it_has_no_readable_text() {
        let dir = temp_dir("pdf-scan");
        // A page that draws a rectangle and no glyph: what a scan looks like to a
        // text extractor.
        let path = write(&dir, "scan.pdf", &sample_pdf("0 0 0 rg 72 700 200 100 re f"));
        let error = read_document_as(&path, "scan.pdf").unwrap_err();
        assert!(
            error.contains("No readable text"),
            "an empty result must not look like an empty document: {error}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_missing_pdftotext_names_the_package_to_install() {
        let dir = temp_dir("pdf-missing-tool");
        let path = write(
            &dir,
            "brief.pdf",
            &sample_pdf("BT /F1 18 Tf 72 700 Td (Hello) Tj ET"),
        );
        // The host without poppler-utils: the same code path a machine that never
        // installed it takes, with nothing removed from this one.
        let error = read_document_with(&path, "brief.pdf", &with_pdftotext(None)).unwrap_err();
        assert!(error.contains("poppler-utils"), "{error}");
        assert!(error.contains("not installed"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn docx_odt_and_epub_yield_text_through_the_zip_path() {
        let dir = temp_dir("containers");
        let docx = container(&[(
            "word/document.xml",
            "<w:document><w:body><w:p><w:r><w:t>Quarterly report</w:t></w:r></w:p>\
             <w:p><w:r><w:t>Revenue &amp; costs</w:t></w:r></w:p></w:body></w:document>",
        )]);
        let document = read_document_as(&write(&dir, "notes.docx", &docx), "notes.docx").unwrap();
        assert_eq!(document.method, "zip");
        assert!(document.text.contains("Quarterly report"), "{}", document.text);
        assert!(document.text.contains("Revenue & costs"), "entities: {}", document.text);

        let odt = container(&[(
            "content.xml",
            "<office:document-content><office:body><office:text>\
             <text:p>Quarterly report</text:p><text:p>Revenue &amp; costs</text:p>\
             </office:text></office:body></office:document-content>",
        )]);
        let document = read_document_as(&write(&dir, "notes.odt", &odt), "notes.odt").unwrap();
        assert_eq!(document.method, "zip");
        assert!(document.text.contains("Quarterly report"), "{}", document.text);

        // Two chapters in the spine, deliberately named so name order and reading
        // order differ in the other direction.
        let epub = container(&[
            (
                "META-INF/container.xml",
                "<container><rootfiles><rootfile full-path=\"OEBPS/content.opf\" \
                 media-type=\"application/oebps-package+xml\"/></rootfiles></container>",
            ),
            (
                "OEBPS/content.opf",
                "<package><manifest>\
                 <item id=\"b\" href=\"chapter2.xhtml\" media-type=\"application/xhtml+xml\"/>\
                 <item id=\"a\" href=\"chapter1.xhtml\" media-type=\"application/xhtml+xml\"/>\
                 </manifest><spine><itemref idref=\"b\"/><itemref idref=\"a\"/></spine></package>",
            ),
            ("OEBPS/chapter1.xhtml", "<html><body><h1>Chapter One</h1><p>The lighthouse keeper</p></body></html>"),
            ("OEBPS/chapter2.xhtml", "<html><body><h1>Chapter Two</h1><p>The harbour wall</p></body></html>"),
        ]);
        let document = read_document_as(&write(&dir, "book.epub", &epub), "book.epub").unwrap();
        assert_eq!(document.method, "zip");
        let two = document.text.find("Chapter Two").expect("second chapter");
        let one = document.text.find("Chapter One").expect("first chapter");
        assert!(two < one, "the spine decides the order: {}", document.text);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_container_without_its_document_part_says_so() {
        let dir = temp_dir("container-empty");
        let misnamed = container(&[("hello.txt", "not a document")]);
        let error =
            read_document_as(&write(&dir, "notes.docx", &misnamed), "notes.docx").unwrap_err();
        assert!(error.contains("no readable document text"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_format_this_host_cannot_read_keeps_the_honest_sentence() {
        let dir = temp_dir("unsupported");
        // Legacy .doc: binary, and no reader for it off macOS.
        let path = write(&dir, "old.doc", b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1\x00\x00\x00\x00");
        let error = read_document_as(&path, "old.doc").unwrap_err();
        assert_eq!(error, UNREADABLE_HERE);
        assert!(error.contains("Attach a text export"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn markup_becomes_readable_text_and_entities_cannot_expand_into_more() {
        let text = xml_text(
            "<w:p><w:r><w:t>Hello</w:t></w:r><w:tab/><w:r><w:t>world &amp; &#65; &#x42; &nbsp;!</w:t></w:r></w:p>\
             <w:p><w:r><w:t>&unknown; stays</w:t></w:r></w:p>",
        );
        let text = tidy_text(&decode_entities(&text));
        assert!(text.contains("Hello\tworld & A B  !"), "{text:?}");
        assert!(text.contains("&unknown; stays"), "{text:?}");
        assert!(!text.contains('<'), "no tag survives: {text:?}");
        // A document cannot express a recursive entity here, because nothing expands
        // a definition: the text is whatever the bytes already spelled.
        assert_eq!(decode_entities("&amp;amp;"), "&amp;");
    }
}
