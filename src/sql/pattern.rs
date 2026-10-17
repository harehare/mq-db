//! Regex and `LIKE` matching.

use regex::Regex;
use rustc_hash::FxHashMap;

thread_local! {
    static REGEX_CACHE: std::cell::RefCell<FxHashMap<String, Option<std::rc::Rc<Regex>>>> =
        std::cell::RefCell::new(FxHashMap::default());
}

pub(super) fn compile_regex(pattern: &str) -> Option<std::rc::Rc<Regex>> {
    REGEX_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(pattern.to_string())
            .or_insert_with(|| Regex::new(pattern).ok().map(std::rc::Rc::new))
            .clone()
    })
}

// LIKE pattern matching (% = .*, _ = any char)
pub(super) fn like_match_str(s: &str, pattern: &str) -> bool {
    let s: Vec<char> = s.to_lowercase().chars().collect();
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    like_dp(&s, &p, 0, 0)
}

fn like_dp(s: &[char], p: &[char], si: usize, pi: usize) -> bool {
    if pi == p.len() {
        return si == s.len();
    }
    if p[pi] == '%' {
        // skip consecutive %
        let mut npi = pi + 1;
        while npi < p.len() && p[npi] == '%' {
            npi += 1;
        }
        for k in si..=s.len() {
            if like_dp(s, p, k, npi) {
                return true;
            }
        }
        return false;
    }
    if si >= s.len() {
        return false;
    }
    let matches = p[pi] == '_' || p[pi] == s[si];
    matches && like_dp(s, p, si + 1, pi + 1)
}
