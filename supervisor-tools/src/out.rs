//! Output helpers for the supervisor tooling binaries.

use std::io::Write as _;

const HTML_PAGE: &str = include_str!("html/page.html");
const HTML_DIAGRAM: &str = include_str!("html/diagram.html");
const HTML_STYLE: &str = include_str!("html/style.css");
const HTML_SCRIPT: &str = include_str!("html/app.js");

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

/// Build a self-contained HTML page that renders the given diagrams.
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
    let script = HTML_SCRIPT.replacen("__SOURCES__", &script_sources(diagrams), 1);
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
    let tmp = std::env::temp_dir().join(format!("supervisor-mermaid-{}.mmd", std::process::id()));
    std::fs::write(&tmp, diagram).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let status = std::process::Command::new("mmdc")
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

/// Update a markdown file between `<!-- supervisor-mermaid:start -->` and
/// `<!-- supervisor-mermaid:end -->` markers.
///
/// If the markers are absent, appends the block to the end of the file.
pub fn update_markdown(path: &str, block: &str) -> Result<(), String> {
    const START: &str = "<!-- supervisor-mermaid:start -->";
    const END: &str = "<!-- supervisor-mermaid:end -->";
    let managed = format!("{START}\n{block}{END}");
    let existing = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let updated = match (existing.find(START), existing.find(END)) {
        (Some(s), Some(e)) if e >= s => {
            let after = e + END.len();
            format!("{}{}{}", &existing[..s], managed, &existing[after..])
        }
        (None, None) => {
            let sep = if existing.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            format!("{existing}{sep}{managed}\n")
        }
        _ => {
            return Err(format!(
                "{path}: found one supervisor-mermaid marker without the other; \
                 fix the markers by hand"
            ));
        }
    };
    std::fs::write(path, updated).map_err(|e| format!("{path}: {e}"))
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
            page.contains("@mermaid-js/layout-elk@0/dist/mermaid-layout-elk.esm.min.mjs"),
            "{page}"
        );
        assert!(
            page.contains("mermaid.registerLayoutLoaders(elkLayouts)"),
            "{page}"
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
}
