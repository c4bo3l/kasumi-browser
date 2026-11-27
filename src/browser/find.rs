//! Rich in-page find bridge.
//!
//! CEF's `FindHandler` is intentionally used for the fast substring path. It
//! does not expose snippets, headings, regular expressions, or multiple match
//! colours, so Kasumi installs a small, idempotent renderer shim after each
//! main-frame load. The shim walks text nodes with a `TreeWalker`, records
//! `Range` objects, paints them through `CSS.highlights` without mutating the
//! DOM, and sends bounded JSON snippets back over the internal console channel
//! already used by the other native UI shims.

use crate::events::FindSnippet;
use serde::Deserialize;
use serde_json::json;

/// Console marker used for rich find results.
pub const FIND_RESULT_CONSOLE_PREFIX: &str = "__KASUMI_FIND_RESULT__:";
/// Upper bound for a console control payload before serde is allowed to build
/// strings from it. The shim itself emits at most 80 snippets.
const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

/// JSON payload emitted by the renderer shim.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindResultPayload {
    #[serde(default)]
    pub request_id: u64,
    #[serde(default)]
    pub count: i32,
    #[serde(default)]
    pub active_ordinal: i32,
    #[serde(default)]
    pub snippets: Vec<FindSnippet>,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub cleared: bool,
}

/// Parse an internal console message emitted by the find shim.
pub fn parse_result(message: &str) -> Option<FindResultPayload> {
    let payload = message.strip_prefix(FIND_RESULT_CONSOLE_PREFIX)?.trim();
    if payload.is_empty() || payload.len() > MAX_PAYLOAD_BYTES {
        return None;
    }
    serde_json::from_str(payload).ok()
}

/// Build the script that runs a new find request. The shim is re-installed on
/// every request so a tab restored or navigated before `on_load_end` still
/// supports find immediately.
pub fn run_script(
    request_id: u64,
    query: &str,
    match_case: bool,
    regex: bool,
    whole_word: bool,
    multi_term: bool,
) -> String {
    let options = json!({
        "requestId": request_id,
        "query": query,
        "matchCase": match_case,
        "regex": regex,
        "wholeWord": whole_word,
        "multiTerm": multi_term,
    });
    format!("{RENDERER_SHIM}\nwindow.__kasumiFind && window.__kasumiFind.run({options});")
}

/// Build the script that removes all highlights and ranges.
pub fn clear_script() -> String {
    format!("{RENDERER_SHIM}\nwindow.__kasumiFind && window.__kasumiFind.clear();")
}

/// Build the script that focuses the one-based match ordinal.
pub fn focus_script(ordinal: i32) -> String {
    let zero_based = ordinal.saturating_sub(1).max(0);
    format!("window.__kasumiFind && window.__kasumiFind.focus({zero_based});")
}

/// Build the script that advances the active rich match.
pub fn next_script(forward: bool) -> String {
    let forward = if forward { "true" } else { "false" };
    format!("window.__kasumiFind && window.__kasumiFind.next({forward});")
}

/// Renderer shim installed by the browser process after main-frame loads and
/// again before every find request. It never mutates the page DOM.
pub const RENDERER_SHIM: &str = r##"
(() => {
  if (window.__kasumiFindShim && window.__kasumiFind && typeof window.__kasumiFind.run === 'function') return;
  window.__kasumiFindShim = 1;

  const PREFIX = '__KASUMI_FIND_RESULT__:';
  const HIGHLIGHT_PREFIX = 'kasumi-find-';
  const ACTIVE_HIGHLIGHT = 'kasumi-find-active';
  const CONTEXT_CHARS = 60;
  const MAX_SNIPPETS = 80;
  const MAX_MATCHES = 500;
  const supportsHighlights = !!(window.CSS && CSS.highlights && window.Highlight);

  const state = {
    requestId: 0,
    ranges: [],
    matches: [],
    active: -1,
    highlightNames: [],
  };

  const post = (payload) => {
    try { console.log(PREFIX + JSON.stringify(payload)); } catch (_) {}
  };
  const escapeRegExp = (value) => value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  const compact = (value) => String(value || '').replace(/\s+/g, ' ').trim();
  const truncate = (value, limit) => {
    const text = compact(value);
    return text.length > limit ? text.slice(0, limit - 1) + '…' : text;
  };

  // Internal resets from run() must not emit a cleared control message: that
  // would race the fresh result payload and make the browser drop the query.
  const clear = (notify = true) => {
    state.ranges = [];
    state.matches = [];
    state.active = -1;
    if (supportsHighlights) {
      for (const name of state.highlightNames) CSS.highlights.delete(name);
      CSS.highlights.delete(ACTIVE_HIGHLIGHT);
    }
    state.highlightNames = [];
    if (notify) {
      post({ requestId: state.requestId, count: 0, activeOrdinal: -1, snippets: [], cleared: true });
    }
  };

  const buildPatterns = (options) => {
    const query = String(options.query || '');
    const flags = options.matchCase ? 'g' : 'gi';
    if (options.regex) return [{ source: query, flags, index: 0 }];
    const pieces = options.multiTerm
      ? query.split(/\s*[|;]\s*/).filter((term) => term.length > 0)
      : [query];
    return pieces.map((term, index) => {
      const source = options.wholeWord ? '\\b(?:' + escapeRegExp(term) + ')\\b' : escapeRegExp(term);
      return { source, flags, index };
    });
  };

  const headingBreadcrumb = (node, parent) => {
    const inside = parent && parent.closest('h1,h2,h3');
    if (inside) return truncate(inside.textContent, 120);
    const headings = Array.from(document.querySelectorAll('h1,h2,h3'));
    const preceding = headings.filter((heading) =>
      !!(node.compareDocumentPosition(heading) & Node.DOCUMENT_POSITION_PRECEDING));
    const last = (level) => preceding.filter((heading) =>
      heading.tagName.toLowerCase() === level).pop();
    return [last('h1'), last('h2'), last('h3')]
      .map((heading) => heading && truncate(heading.textContent, 80))
      .filter(Boolean)
      .join(' › ');
  };

  const contextSentence = (text, start, end) => {
    const left = Math.max(text.lastIndexOf('.', Math.max(0, start - 1)),
      text.lastIndexOf('!', Math.max(0, start - 1)),
      text.lastIndexOf('?', Math.max(0, start - 1))) + 1;
    const rightCandidates = ['.', '!', '?'].map((mark) => text.indexOf(mark, end));
    const right = rightCandidates.filter((index) => index >= 0);
    const stop = right.length ? Math.min.apply(null, right) + 1 : text.length;
    return compact(text.slice(left, stop));
  };

  const makeSnippet = (match, ordinal) => {
    const node = match.range.startContainer;
    const parent = node.parentElement;
    const text = String(node.nodeValue || '');
    const start = match.range.startOffset;
    const end = match.range.endOffset;
    const line = compact(text);
    const anchor = parent && parent.closest('a');
    return {
      ordinal: ordinal,
      before: compact(text.slice(Math.max(0, start - CONTEXT_CHARS), start)),
      matched: compact(text.slice(start, end)),
      after: compact(text.slice(end, end + CONTEXT_CHARS)),
      heading: headingBreadcrumb(node, parent),
      href: anchor ? String(anchor.href || '') : '',
      line: truncate(line, 360),
      sentence: truncate(contextSentence(text, start, end), 360),
      termIndex: match.termIndex,
    };
  };

  const applyHighlights = () => {
    if (!supportsHighlights || state.matches.length === 0) return;
    const byTerm = new Map();
    for (const match of state.matches) {
      if (!byTerm.has(match.termIndex)) byTerm.set(match.termIndex, []);
      byTerm.get(match.termIndex).push(match.range);
    }
    for (const [termIndex, ranges] of byTerm.entries()) {
      const name = HIGHLIGHT_PREFIX + termIndex;
      try {
        CSS.highlights.set(name, new Highlight(...ranges));
        state.highlightNames.push(name);
      } catch (_) {}
    }
  };

  const focus = (index) => {
    if (!state.matches.length) return;
    const next = Math.max(0, Math.min(state.matches.length - 1, index));
    state.active = next;
    const match = state.matches[next];
    if (supportsHighlights) {
      CSS.highlights.delete(ACTIVE_HIGHLIGHT);
      try {
        CSS.highlights.set(ACTIVE_HIGHLIGHT, new Highlight(match.range));
      } catch (_) {}
    }
    try {
      const element = match.range.startContainer.parentElement;
      if (element && element.scrollIntoView) {
        element.scrollIntoView({ block: 'center', inline: 'nearest' });
      }
      const selection = window.getSelection();
      if (selection) {
        selection.removeAllRanges();
        selection.addRange(match.range);
      }
    } catch (_) {}
    post({
      requestId: state.requestId,
      count: state.matches.length,
      activeOrdinal: next + 1,
      snippets: [],
      cleared: false,
    });
  };

  const run = (options) => {
    state.requestId = Number(options.requestId || 0);
    clear(false);
    state.requestId = Number(options.requestId || 0);
    const query = String(options.query || '');
    if (!query.trim()) return;

    let patterns;
    try {
      patterns = buildPatterns(options);
    } catch (error) {
      post({
        requestId: state.requestId,
        count: 0,
        activeOrdinal: -1,
        snippets: [],
        error: String(error && error.message ? error.message : error),
        cleared: false,
      });
      return;
    }

    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT, {
      acceptNode(node) {
        const parent = node.parentElement;
        if (!parent || !node.nodeValue || !node.nodeValue.trim()) return NodeFilter.FILTER_REJECT;
        if (parent.closest('script,style,noscript,template')) return NodeFilter.FILTER_REJECT;
        return NodeFilter.FILTER_ACCEPT;
      },
    });

    let node = walker.nextNode();
    while (node && state.matches.length < MAX_MATCHES) {
      const text = String(node.nodeValue || '');
      for (const pattern of patterns) {
        if (!pattern.source) continue;
        let regex;
        try {
          regex = new RegExp(pattern.source, pattern.flags);
        } catch (error) {
          post({
            requestId: state.requestId,
            count: state.matches.length,
            activeOrdinal: -1,
            snippets: [],
            error: String(error && error.message ? error.message : error),
            cleared: false,
          });
          return;
        }
        let found;
        while ((found = regex.exec(text)) !== null) {
          const matched = found[0];
          if (!matched.length) {
            regex.lastIndex += 1;
            continue;
          }
          const range = document.createRange();
          range.setStart(node, found.index);
          range.setEnd(node, found.index + matched.length);
          state.ranges.push(range);
          state.matches.push({ range: range, termIndex: pattern.index });
          if (state.matches.length >= MAX_MATCHES) break;
        }
        if (state.matches.length >= MAX_MATCHES) break;
      }
      node = walker.nextNode();
    }

    applyHighlights();
    post({
      requestId: state.requestId,
      count: state.matches.length,
      activeOrdinal: state.matches.length ? 1 : -1,
      snippets: state.matches.slice(0, MAX_SNIPPETS).map(makeSnippet),
      error: '',
      cleared: false,
    });
    if (state.matches.length) focus(0);
  };

  const next = (forward) => {
    if (!state.matches.length) return;
    const base = state.active < 0 ? (forward ? -1 : 0) : state.active;
    const length = state.matches.length;
    focus((base + (forward ? 1 : -1) + length) % length);
  };

  window.__kasumiFind = { run: run, clear: clear, focus: focus, next: next };
})();
"##;

#[cfg(test)]
mod tests {
    use super::{clear_script, focus_script, next_script, parse_result, run_script, RENDERER_SHIM};

    #[test]
    fn parses_bounded_rich_find_payloads() {
        let message = concat!(
            "__KASUMI_FIND_RESULT__:",
            r#"{"requestId":7,"count":2,"activeOrdinal":1,"snippets":[],"error":"","cleared":false}"#
        );
        let payload = parse_result(message).expect("valid payload");
        assert_eq!(payload.request_id, 7);
        assert_eq!(payload.count, 2);
        assert_eq!(payload.active_ordinal, 1);
        assert!(parse_result("ordinary page logging").is_none());
    }

    #[test]
    fn rejects_oversized_payloads() {
        let oversized = format!(
            "__KASUMI_FIND_RESULT__:{}",
            "x".repeat(super::MAX_PAYLOAD_BYTES + 1)
        );
        assert!(parse_result(&oversized).is_none());
    }

    #[test]
    fn renderer_scripts_target_the_installed_shim() {
        let run = run_script(3, "error|timeout", true, false, false, true);
        assert!(run.contains("__kasumiFind.run("));
        assert!(run.contains("requestId"));
        assert!(clear_script().contains("__kasumiFind.clear()"));
        assert!(
            run.contains("clear(false)"),
            "internal reset must stay silent"
        );
        assert!(focus_script(4).contains("focus(3)"));
        assert!(next_script(true).contains("next(true)"));
        assert!(RENDERER_SHIM.contains("CSS.highlights"));
        assert!(RENDERER_SHIM.contains("createTreeWalker"));
    }
}
