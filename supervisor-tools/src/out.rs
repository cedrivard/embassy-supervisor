//! Output helpers for the supervisor tooling binaries.

use std::io::Write as _;

const HTML_PAGE: &str = include_str!("html/page.html");
const HTML_DIAGRAM: &str = include_str!("html/diagram.html");
const HTML_STYLE: &str = include_str!("html/style.css");
const HTML_SCRIPT: &str = include_str!("html/app.js");

/// The mermaid release `--html` pages load, pinned so a page renders the
/// same diagram whenever it is opened.
pub const MERMAID_VERSION: &str = "11.17.2";
/// The `@mermaid-js/layout-elk` release a page loads for a `layout: elk` diagram.
pub const ELK_VERSION: &str = "0.2.3";

/// Encode a Mermaid diagram into a shareable mermaid.live URL.
pub fn live_url(diagram: &str) -> String {
    let state = serde_json::json!({
        "code": diagram,
        "mermaid": { "theme": "default" },
        "autoSync": true,
        "updateDiagram": false,
    });
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    let _ = enc.write_all(state.to_string().as_bytes());
    let bytes = enc.finish().unwrap_or_default();
    format!("https://mermaid.live/edit#pako:{}", base64_url(&bytes))
}

fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Build an HTML page that renders the given diagrams. The page loads mermaid
/// (and, for a `layout: elk` diagram, its ELK layout) from a CDN at view time.
///
/// `diagrams` is a list of `(heading, mermaid_source)` pairs.
pub fn html_page(title: Option<&str>, diagrams: &[(String, String)]) -> String {
    let body = diagram_sections(diagrams);
    let title = title.map(html_esc);
    let browser_title = title
        .as_deref()
        .map(|title| format!("<title>{title}</title>\n"))
        .unwrap_or_default();
    let heading = title
        .as_deref()
        .map(|title| format!("<h1>{title}</h1>\n"))
        .unwrap_or_default();
    let script = HTML_SCRIPT
        .replacen("__MERMAID_VERSION__", MERMAID_VERSION, 1)
        .replacen("__ELK_VERSION__", ELK_VERSION, 1)
        .replacen("__SOURCES__", &script_sources(diagrams), 1);
    HTML_PAGE
        .replacen("{{STYLE}}", HTML_STYLE, 1)
        .replacen("{{SCRIPT}}", &script, 1)
        .replacen("{{DIAGRAMS}}", &body, 1)
        .replacen("{{PAGE_HEADING}}", &heading, 1)
        .replacen("{{DOCUMENT_TITLE}}", &browser_title, 1)
}

fn diagram_sections(diagrams: &[(String, String)]) -> String {
    diagrams
        .iter()
        .enumerate()
        .map(|(index, (heading, _))| {
            HTML_DIAGRAM
                .replace("{{INDEX}}", &index.to_string())
                .replacen("{{HEADING}}", &html_esc(heading), 1)
        })
        .collect()
}

fn script_sources(diagrams: &[(String, String)]) -> String {
    let sources: Vec<&str> = diagrams.iter().map(|(_, code)| code.as_str()).collect();
    serde_json::to_string(&sources)
        .expect("serializing Mermaid diagram sources")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026")
}

fn html_esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render a diagram to an image using the `mmdc` command-line tool.
///
/// Returns an error if `mmdc` is not installed or exits unsuccessfully.
pub fn mmdc_render(diagram: &str, out_path: &str) -> Result<(), String> {
    let tmp = mmdc_input(diagram)?;
    // npm installs a `.cmd` shim on Windows, which `Command` does not find
    // under the bare name.
    let program = if cfg!(windows) { "mmdc.cmd" } else { "mmdc" };
    let status = std::process::Command::new(program)
        .arg("-i")
        .arg(&tmp)
        .args(["-o", out_path])
        .status();
    let _ = std::fs::remove_file(&tmp);
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => Err(format!("mmdc exited with {s}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(
            "`mmdc` (mermaid-cli) is not installed; `npm install -g @mermaid-js/mermaid-cli`, \
             or use --html / --live-url, which need nothing"
                .to_string(),
        ),
        Err(e) => Err(format!("running mmdc: {e}")),
    }
}

/// Write `diagram` to a fresh temp file for `mmdc`. Named by pid and clock and
/// created exclusively, so a file or symlink already at the path is an error,
/// never a target.
fn mmdc_input(diagram: &str) -> Result<std::path::PathBuf, String> {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = std::env::temp_dir().join(format!("supervisor-mermaid-{pid}-{nanos}.mmd"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| format!("{}: {e}", tmp.display()))?;
    if let Err(e) = f.write_all(diagram.as_bytes()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {e}", tmp.display()));
    }
    Ok(tmp)
}

const START: &str = "<!-- supervisor-mermaid:start -->";
const END: &str = "<!-- supervisor-mermaid:end -->";

/// Update a markdown file between `<!-- supervisor-mermaid:start -->` and
/// `<!-- supervisor-mermaid:end -->` marker lines.
///
/// A marker counts only alone on its line and outside a fenced code block, so
/// one quoted in prose, a table or an example is text. With no markers, the
/// block is appended to the end of the file. The block takes the file's line
/// ending (CRLF when its first line ends so). A file that would not change is
/// not written; otherwise the new text goes to a temporary file beside it,
/// which is then renamed over it.
pub fn update_markdown(path: &str, block: &str) -> Result<(), String> {
    let existing = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let crlf = existing
        .find('\n')
        .is_some_and(|i| existing[..i].ends_with('\r'));
    let nl = if crlf { "\r\n" } else { "\n" };
    let managed = format!("{START}\n{block}{END}")
        .replace("\r\n", "\n")
        .replace('\n', nl);
    let updated = splice(&existing, &managed, nl).map_err(|e| format!("{path}: {e}"))?;
    if updated == existing {
        return Ok(());
    }
    let tmp = format!("{path}.supervisor-mermaid.tmp");
    let replaced = std::fs::write(&tmp, &updated).and_then(|()| std::fs::rename(&tmp, path));
    replaced.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{path}: {e}")
    })
}

/// `existing` with `managed` in place of its marked block, or appended after
/// a line ending `nl`.
fn splice(existing: &str, managed: &str, nl: &str) -> Result<String, String> {
    match (marker_line(existing, START), marker_line(existing, END)) {
        (Some((s, _)), Some((e, after))) if e > s => {
            if marker_line(&existing[after..], START).is_some() {
                return Err(
                    "a second supervisor-mermaid start marker follows the managed block; \
                     remove one"
                        .to_string(),
                );
            }
            Ok(format!("{}{managed}{}", &existing[..s], &existing[after..]))
        }
        (Some(_), Some(_)) => Err("the supervisor-mermaid end marker comes before the start \
                                   marker; fix the markers by hand"
            .to_string()),
        (None, None) => {
            let sep = if existing.ends_with('\n') {
                nl.to_string()
            } else {
                format!("{nl}{nl}")
            };
            Ok(format!("{existing}{sep}{managed}{nl}"))
        }
        _ => Err(
            "found one supervisor-mermaid marker without the other; fix the markers by hand"
                .to_string(),
        ),
    }
}

/// The byte range of `marker` on the first line that holds it alone
/// (surrounding whitespace allowed), outside a fenced code block.
fn marker_line(text: &str, marker: &str) -> Option<(usize, usize)> {
    let mut at = 0;
    // The open fence: its character and length.
    let mut fence: Option<(char, usize)> = None;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let run = |c: char| trimmed.chars().take_while(|&x| x == c).count();
        let (tick, tilde) = (run('`'), run('~'));
        let opener = if tick >= 3 {
            Some(('`', tick))
        } else if tilde >= 3 {
            Some(('~', tilde))
        } else {
            None
        };
        match (fence, opener) {
            (None, Some(open)) => fence = Some(open),
            (Some((c, n)), Some((d, m))) if c == d && m >= n && trimmed[m..].trim().is_empty() => {
                fence = None;
            }
            (None, None) if line.trim() == marker => {
                let lead = line.len() - trimmed.len();
                return Some((at + lead, at + lead + marker.len()));
            }
            _ => {}
        }
        at += line.len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_url_roundtrips() {
        use std::io::Read;
        let url = live_url("flowchart TD\n  a --> b\n");
        let b64 = url.strip_prefix("https://mermaid.live/edit#pako:").unwrap();
        let bytes = debase64_url(b64);
        let mut inflater = flate2::read::ZlibDecoder::new(bytes.as_slice());
        let mut json = String::new();
        inflater.read_to_string(&mut json).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["code"], "flowchart TD\n  a --> b\n");
    }

    #[test]
    fn html_page_shows_or_omits_its_title() {
        let diagrams = [("graph".to_string(), "flowchart TD\n".to_string())];
        let titled = html_page(Some("Firmware <graph>"), &diagrams);
        assert!(
            titled.contains("<title>Firmware &lt;graph&gt;</title>"),
            "{titled}"
        );
        assert!(
            titled.contains("<h1>Firmware &lt;graph&gt;</h1>"),
            "{titled}"
        );

        let untitled = html_page(None, &diagrams);
        assert!(!untitled.contains("<title>"), "{untitled}");
        assert!(!untitled.contains("<h1>"), "{untitled}");
    }

    #[test]
    fn html_page_has_a_persisted_system_light_dark_switcher() {
        let diagrams = [(
            "graph".to_string(),
            "flowchart TD\nA[</script>]\n".to_string(),
        )];
        let page = html_page(Some("Graph"), &diagrams);
        for mode in ["system", "light", "dark"] {
            assert!(
                page.contains(&format!("data-theme-mode=\"{mode}\"")),
                "missing {mode} control: {page}"
            );
        }
        assert!(
            page.contains("matchMedia(\"(prefers-color-scheme: dark)\")"),
            "{page}"
        );
        assert!(page.contains("supervisor-mermaid:appearance"), "{page}");
        assert!(page.contains("mermaid.render("), "{page}");
        assert!(page.contains("theme: resolvedTheme()"), "{page}");
        assert!(
            page.contains(&format!(
                "npm/mermaid@{MERMAID_VERSION}/dist/mermaid.esm.min.mjs"
            )),
            "{page}"
        );
        assert!(!page.contains("__"), "every placeholder is filled: {page}");
        assert!(
            page.contains(&format!(
                "@mermaid-js/layout-elk@{ELK_VERSION}/dist/mermaid-layout-elk.esm.min.mjs"
            )),
            "{page}"
        );
        assert!(
            page.contains("mermaid.registerLayoutLoaders(elkLayouts)"),
            "{page}"
        );
        assert!(
            page.contains("await import("),
            "ELK loads only when a diagram asks for it: {page}"
        );
        assert!(page.contains("A[\\u003c/script\\u003e]"), "{page}");
        assert!(page.contains(".page-header {"), "{page}");
        assert!(page.contains("<section class=\"diagram\">"), "{page}");
        assert!(
            page.contains("data-index=\"0\" aria-labelledby=\"diagram-0-title\""),
            "{page}"
        );
    }

    fn debase64_url(s: &str) -> Vec<u8> {
        let val = |c: u8| -> u32 {
            match c {
                b'A'..=b'Z' => (c - b'A') as u32,
                b'a'..=b'z' => (c - b'a' + 26) as u32,
                b'0'..=b'9' => (c - b'0' + 52) as u32,
                b'-' => 62,
                b'_' => 63,
                _ => 0,
            }
        };
        let s: Vec<u8> = s.bytes().filter(|&c| c != b'=').collect();
        let mut out = Vec::new();
        for chunk in s.chunks(4) {
            let mut n = 0u32;
            for (i, &c) in chunk.iter().enumerate() {
                n |= val(c) << (18 - 6 * i);
            }
            let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
            out.extend_from_slice(&bytes[..chunk.len() - 1]);
        }
        out
    }

    #[test]
    fn markdown_update_replaces_and_appends() {
        let p = std::env::temp_dir().join(format!("svm_md_{}.md", std::process::id()));
        let path = p.display().to_string();
        std::fs::write(&p, "# Doc\n\ntext\n").unwrap();
        update_markdown(&path, "ONE\n").unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("# Doc"), "{s}");
        assert!(s.contains(":start -->\nONE\n<!--"), "{s}");
        update_markdown(&path, "TWO\n").unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(!s.contains("ONE"), "{s}");
        assert_eq!(s.matches("supervisor-mermaid:start").count(), 1, "{s}");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn markdown_update_writes_only_a_change_and_keeps_crlf() {
        let p = std::env::temp_dir().join(format!("svm_md_crlf_{}.md", std::process::id()));
        let path = p.display().to_string();
        std::fs::write(&p, "# Doc\r\n\r\ntext\r\n").unwrap();
        update_markdown(&path, "ONE\n").unwrap();
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(!s.replace("\r\n", "").contains('\n'), "{s:?}");
        assert!(s.contains(":start -->\r\nONE\r\n<!--"), "{s:?}");

        // With the temporary path blocked, only a run that changes nothing
        // can succeed: it writes nothing.
        let tmp = format!("{path}.supervisor-mermaid.tmp");
        std::fs::create_dir(&tmp).unwrap();
        update_markdown(&path, "ONE\n").unwrap();
        assert!(update_markdown(&path, "TWO\n").is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), s);
        std::fs::remove_dir(&tmp).ok();
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn markers_in_a_fence_are_text_and_a_second_start_is_refused() {
        let managed = "<!-- supervisor-mermaid:start -->\nX\n<!-- supervisor-mermaid:end -->";
        let example = "````md\n<!-- supervisor-mermaid:start -->\n```\n````\n\
                       ~~~\n<!-- supervisor-mermaid:end -->\n~~~\n";
        assert_eq!(
            splice(example, managed, "\n").unwrap(),
            format!("{example}\n{managed}\n")
        );
        let twice = splice(
            "<!-- supervisor-mermaid:start -->\nold\n<!-- supervisor-mermaid:end -->\n\
             <!-- supervisor-mermaid:start -->\n",
            managed,
            "\n",
        );
        assert!(
            twice
                .as_ref()
                .is_err_and(|e| e.contains("a second supervisor-mermaid start marker")),
            "{twice:?}"
        );
    }

    #[test]
    fn markers_count_only_as_whole_lines() {
        let managed = "<!-- supervisor-mermaid:start -->\nX\n<!-- supervisor-mermaid:end -->";
        let quoted = "| `--update FILE.md` | rewrite the block between the \
                      `<!-- supervisor-mermaid:start -->` and \
                      `<!-- supervisor-mermaid:end -->` lines in a markdown file, \
                      touching nothing else |\n";
        assert_eq!(
            splice(quoted, managed, "\n").unwrap(),
            format!("{quoted}\n{managed}\n")
        );
        let swapped = splice(
            "<!-- supervisor-mermaid:end -->\n<!-- supervisor-mermaid:start -->\n",
            managed,
            "\n",
        );
        assert!(
            swapped
                .as_ref()
                .is_err_and(|e| e.contains("before the start")),
            "{swapped:?}"
        );
        let lone = splice("<!-- supervisor-mermaid:start -->\n", managed, "\n");
        assert!(
            lone.as_ref()
                .is_err_and(|e| e.contains("without the other")),
            "{lone:?}"
        );
        assert_eq!(
            splice(
                "  <!-- supervisor-mermaid:start -->\nold\n<!-- supervisor-mermaid:end -->\n",
                managed,
                "\n"
            )
            .unwrap(),
            format!("  {managed}\n")
        );
    }

    #[test]
    fn the_mmdc_input_is_a_fresh_file() {
        let tmp = mmdc_input("flowchart TD\n").unwrap();
        let name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.starts_with(&format!("supervisor-mermaid-{}-", std::process::id())),
            "{name}"
        );
        assert!(name.ends_with(".mmd"), "{name}");
        assert_eq!(std::fs::read_to_string(&tmp).unwrap(), "flowchart TD\n");
        std::fs::remove_file(&tmp).unwrap();
    }
}
