//! Parsing of DuckDuckGo HTML search results.
//!
//! `WebSearch` fetches `https://html.duckduckgo.com/html/?q=<query>`
//! through the helper and this module turns the returned page into a
//! numbered text listing. Result links are anchors with class
//! `result__a`; their `href` wraps the target URL in a `uddg=`
//! redirect parameter (percent-encoded). Snippets carry class
//! `result__snippet`.

/// Maximum number of results included in the listing.
const MAX_RESULTS: usize = 10;

#[derive(Debug, PartialEq)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Extracts up to ten results from a DuckDuckGo HTML page and formats
/// them as:
///
/// ```text
/// 1. <title>
///    <url>
///    <snippet>
/// ```
///
/// Returns `"No results."` when the page contains no result links.
pub fn parse_results(html: &str) -> String {
    let results = extract_results(html);
    if results.is_empty() {
        return "No results.".to_string();
    }
    let entries: Vec<String> = results
        .iter()
        .enumerate()
        .map(|(index, result)| {
            let mut entry = format!("{}. {}\n   {}", index + 1, result.title, result.url);
            if !result.snippet.is_empty() {
                entry.push_str("\n   ");
                entry.push_str(&result.snippet);
            }
            entry
        })
        .collect();
    entries.join("\n\n")
}

fn extract_results(html: &str) -> Vec<SearchResult> {
    let mut results = Vec::new();
    let mut cursor = 0;
    while results.len() < MAX_RESULTS {
        let Some(anchor) = find_class_tag(html, cursor, "result__a") else {
            break;
        };
        let Some(tag_end) = html[anchor.tag_start..]
            .find('>')
            .map(|i| anchor.tag_start + i + 1)
        else {
            break;
        };
        let Some(close) = html[tag_end..].find("</a>").map(|i| tag_end + i) else {
            break;
        };
        let title = clean_text(&html[tag_end..close]);
        let url = attr_value(&html[anchor.tag_start..tag_end], "href")
            .map(|href| result_url(&href))
            .unwrap_or_default();
        let after_anchor = close + 4;

        // The snippet belongs to this result only if it appears before the
        // next result link.
        let next_anchor_at = find_class_tag(html, after_anchor, "result__a")
            .map(|a| a.class_at)
            .unwrap_or(html.len());
        let snippet = find_class_tag(html, after_anchor, "result__snippet")
            .filter(|s| s.class_at < next_anchor_at)
            .and_then(|s| {
                let content_start = html[s.tag_start..].find('>').map(|i| s.tag_start + i + 1)?;
                let content_end = ["</a>", "</div>", "</td>", "</span>", "</p>"]
                    .iter()
                    .filter_map(|closer| html[content_start..].find(closer))
                    .min()
                    .map(|i| content_start + i)?;
                Some(clean_text(&html[content_start..content_end]))
            })
            .unwrap_or_default();

        if !title.is_empty() && !url.is_empty() {
            results.push(SearchResult {
                title,
                url,
                snippet,
            });
        }
        cursor = after_anchor;
    }
    results
}

struct ClassTag {
    /// Offset of the class-name occurrence.
    class_at: usize,
    /// Offset of the `<` opening the tag that carries the class.
    tag_start: usize,
}

/// Finds the next tag at or after `from` whose `class` attribute contains
/// `class_name` as a whole word.
fn find_class_tag(html: &str, from: usize, class_name: &str) -> Option<ClassTag> {
    let mut search_from = from;
    while let Some(relative) = html[search_from..].find(class_name) {
        let class_at = search_from + relative;
        let after = html[class_at + class_name.len()..].chars().next();
        let is_word_end = !matches!(
            after,
            Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-'
        );
        if is_word_end {
            if let Some(tag_start) = html[..class_at].rfind('<') {
                // The occurrence must be inside the tag (no '>' between).
                if !html[tag_start..class_at].contains('>') {
                    return Some(ClassTag {
                        class_at,
                        tag_start,
                    });
                }
            }
        }
        search_from = class_at + class_name.len();
    }
    None
}

/// Extracts the value of `name="..."` from a tag, with entities decoded.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let marker = format!("{name}=\"");
    let start = tag.find(&marker)? + marker.len();
    let end = tag[start..].find('"')? + start;
    Some(decode_entities(&tag[start..end]))
}

/// Turns a result link href into the target URL: DuckDuckGo wraps targets
/// in a redirect with the destination percent-encoded in the `uddg`
/// parameter.
fn result_url(href: &str) -> String {
    if let Some(at) = href.find("uddg=") {
        let value = &href[at + 5..];
        let end = value.find('&').unwrap_or(value.len());
        return percent_decode(&value[..end]);
    }
    if let Some(rest) = href.strip_prefix("//") {
        return format!("https://{rest}");
    }
    href.to_string()
}

/// Removes tags, decodes entities, and collapses whitespace runs.
fn clean_text(fragment: &str) -> String {
    let mut without_tags = String::with_capacity(fragment.len());
    let mut in_tag = false;
    for c in fragment.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => without_tags.push(c),
            _ => {}
        }
    }
    let decoded = decode_entities(&without_tags);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Percent-decodes a string (leaves `+` and malformed sequences as-is).
pub(crate) fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if let (Some(hi), Some(lo)) = (
                bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16)),
                bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16)),
            ) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-encodes a string for use in a URL query value.
pub(crate) fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture mirroring the html.duckduckgo.com result
    /// markup: redirect-wrapped hrefs, entities in titles, markup in
    /// snippets.
    const RESULTS_PAGE: &str = r#"<!DOCTYPE html>
<html><head><title>q at DuckDuckGo</title></head>
<body>
<div class="serp__results">
  <div class="result results_links results_links_deep web-result">
    <div class="links_main links_deep result__body">
      <h2 class="result__title">
        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust%2Dlang.org%2F&amp;rut=abc123">Rust Programming Language</a>
      </h2>
      <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rust%2Dlang.org%2F&amp;rut=abc123">A language empowering everyone to build <b>reliable</b> and efficient software.</a>
    </div>
  </div>
  <div class="result results_links results_links_deep web-result">
    <div class="links_main links_deep result__body">
      <h2 class="result__title">
        <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2Fbook%2F&amp;rut=def456">The Rust Book &amp; Guide</a>
      </h2>
      <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust%2Dlang.org%2Fbook%2F&amp;rut=def456">Learn Rust with the &quot;book&quot;.</a>
    </div>
  </div>
  <div class="result results_links results_links_deep web-result">
    <div class="links_main links_deep result__body">
      <h2 class="result__title">
        <a rel="nofollow" class="result__a" href="https://example.com/direct">Direct Link Result</a>
      </h2>
    </div>
  </div>
</div>
</body></html>"#;

    const EMPTY_PAGE: &str = r#"<html><body>
<div class="no-results">No results found for your search.</div>
</body></html>"#;

    #[test]
    fn parses_titles_urls_and_snippets() {
        let listing = parse_results(RESULTS_PAGE);
        let expected = "\
1. Rust Programming Language
   https://www.rust-lang.org/
   A language empowering everyone to build reliable and efficient software.

2. The Rust Book & Guide
   https://doc.rust-lang.org/book/
   Learn Rust with the \"book\".

3. Direct Link Result
   https://example.com/direct";
        assert_eq!(listing, expected);
    }

    #[test]
    fn empty_page_yields_no_results() {
        assert_eq!(parse_results(EMPTY_PAGE), "No results.");
        assert_eq!(parse_results(""), "No results.");
    }

    #[test]
    fn caps_at_ten_results() {
        let mut page = String::new();
        for i in 0..15 {
            page.push_str(&format!(
                "<a class=\"result__a\" href=\"//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F{i}&amp;rut=r\">Result {i}</a>\n"
            ));
        }
        let listing = parse_results(&page);
        assert!(listing.contains("10. Result 9"));
        assert!(!listing.contains("11. Result 10"));
    }

    #[test]
    fn snippet_does_not_leak_into_the_previous_result() {
        // The first result has no snippet; the second result's snippet
        // must not be attributed to the first.
        let page = "\
<a class=\"result__a\" href=\"https://one.example\">One</a>
<a class=\"result__a\" href=\"https://two.example\">Two</a>
<a class=\"result__snippet\" href=\"https://two.example\">Snippet for two.</a>";
        let listing = parse_results(page);
        let expected = "\
1. One
   https://one.example

2. Two
   https://two.example
   Snippet for two.";
        assert_eq!(listing, expected);
    }

    #[test]
    fn result_url_unwraps_redirects() {
        assert_eq!(
            result_url("//duckduckgo.com/l/?uddg=https%3A%2F%2Fa.example%2Fpath%3Fq%3D1&rut=zzz"),
            "https://a.example/path?q=1"
        );
        assert_eq!(result_url("//bare.example/x"), "https://bare.example/x");
        assert_eq!(
            result_url("https://plain.example/"),
            "https://plain.example/"
        );
    }

    #[test]
    fn percent_codec_roundtrips() {
        let original = "rust async fn & lifetimes? (2026)";
        let encoded = percent_encode(original);
        assert_eq!(
            encoded,
            "rust%20async%20fn%20%26%20lifetimes%3F%20%282026%29"
        );
        assert_eq!(percent_decode(&encoded), original);
        // Malformed escapes survive unchanged.
        assert_eq!(percent_decode("100% sure%2"), "100% sure%2");
    }

    #[test]
    fn entities_are_decoded_once() {
        assert_eq!(decode_entities("a &amp;lt; b"), "a &lt; b");
        assert_eq!(decode_entities("x &lt; y &amp; z"), "x < y & z");
    }
}
