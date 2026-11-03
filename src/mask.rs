//! Masks secrets in text before it reaches the wall display.
//!
//! - URLs are cut to their host: `https://hooks.example.com/a/b?t=x` → `hooks.example.com/…`.
//! - Token-like words become `••••`: known key prefixes, or long runs of
//!   letters+digits (20+ chars, containing both).
//!
//! Callers are told never to send secrets; this is defence in depth.

const MASK: &str = "••••";
const KNOWN_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "pk_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "AKIA",
    "AIza",
    "ya29.",
    "hf_",
];

pub fn mask(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();
    for c in text.chars() {
        if c.is_whitespace() {
            out.push_str(&mask_word(&word));
            word.clear();
            out.push(c);
        } else {
            word.push(c);
        }
    }
    out.push_str(&mask_word(&word));
    out
}

fn mask_word(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    // Keep surrounding punctuation like "(…)" or a trailing comma.
    let start = word
        .find(|c: char| !matches!(c, '(' | '[' | '<' | '"' | '\'' | '`'))
        .unwrap_or(word.len());
    let end = word
        .rfind(|c: char| {
            !matches!(
                c,
                ')' | ']' | '>' | '"' | '\'' | '`' | ',' | '.' | ';' | ':' | '!' | '?'
            )
        })
        .map(|i| i + word[i..].chars().next().map_or(1, char::len_utf8))
        .unwrap_or(start)
        .max(start);
    let (pre, core, post) = (&word[..start], &word[start..end], &word[end..]);
    let masked = if let Some((host, rest)) = url_parts(core) {
        if rest.trim_matches('/').is_empty() {
            host.to_owned()
        } else {
            format!("{host}/…")
        }
    } else if let Some((k, _)) = core.split_once('=').filter(|(_, v)| looks_like_token(v)) {
        // Secrets also hide in key=value pairs: `token=abcd...`.
        format!("{k}={MASK}")
    } else if looks_like_token(core) {
        MASK.to_owned()
    } else {
        core.to_owned()
    };
    format!("{pre}{masked}{post}")
}

/// Splits a URL into (host, everything after the host).
fn url_parts(s: &str) -> Option<(&str, &str)> {
    let rest = s
        .strip_prefix("https://")
        .or_else(|| s.strip_prefix("http://"))?;
    // Drop any user:password@ part.
    let rest = match rest.find('@') {
        Some(at) if !rest[..at].contains('/') => &rest[at + 1..],
        _ => rest,
    };
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (host, tail) = rest.split_at(host_end);
    (!host.is_empty()).then_some((host, tail))
}

fn looks_like_token(s: &str) -> bool {
    if KNOWN_PREFIXES
        .iter()
        .any(|p| s.starts_with(p) && s.len() >= p.len() + 8)
    {
        return true;
    }
    if s.chars().count() < 20 {
        return false;
    }
    let allowed = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '/' | '=' | '.'));
    let digits = s.chars().filter(char::is_ascii_digit).count();
    let letters = s.chars().filter(char::is_ascii_alphabetic).count();
    allowed && digits >= 2 && letters >= 2 && !s.contains("://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_keep_only_the_host() {
        assert_eq!(
            mask("post to https://hooks.slack.com/services/T000/B000/XXXX now"),
            "post to hooks.slack.com/… now"
        );
        assert_eq!(mask("see https://example.com"), "see example.com");
        assert_eq!(mask("(https://u:pw@host.io/x?k=1)."), "(host.io/…).");
        assert_eq!(mask("https://github.com/o/r/pull/12"), "github.com/…");
    }

    #[test]
    fn tokens_are_masked() {
        assert_eq!(mask("key sk-ant-api03-abcdefghijklmnop"), "key ••••");
        assert_eq!(mask("ghp_1234567890abcdefghijABCDEFGHIJ"), "••••");
        assert_eq!(mask("token=4f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c"), "token=••••");
        assert_eq!(mask("AKIAIOSFODNN7EXAMPLE"), "••••");
    }

    #[test]
    fn ordinary_text_is_untouched() {
        for s in [
            "Fix the login bug in ideaconnect/hermes-beaver#12",
            "commit 3f9a2b1 broke CI",
            "Refactor authentication_middleware_handler",
            "version 1.18.1 released",
            "",
            "multi  space\nnewline",
        ] {
            assert_eq!(mask(s), s, "{s}");
        }
    }
}
