//! Tolerant, anti-clobber text replacement for generic edit adapters.
//!
//! The cascade starts exact and grows progressively more tolerant of line
//! endings, indentation, whitespace, and escaped text. It never silently
//! chooses an ambiguous span, and a disproportionate-match guard prevents a
//! fuzzy anchor from replacing far more content than the caller supplied.

pub const EMPTY_OLD_STRING_MESSAGE: &str = "old_string cannot be empty when editing an existing file. Provide the exact text to replace, or use write_file for an intentional full-file replacement.";

const SINGLE_CANDIDATE_SIMILARITY_THRESHOLD: f64 = 0.65;
const MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD: f64 = 0.65;

pub fn normalize_line_endings(text: &str) -> String {
    text.replace("\r\n", "\n")
}

pub fn detect_line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

pub fn convert_to_line_ending(text: &str, ending: &str) -> String {
    if ending == "\n" {
        text.to_string()
    } else {
        text.replace('\n', "\r\n")
    }
}

fn levenshtein(a: &[char], b: &[char]) -> usize {
    if a.is_empty() || b.is_empty() {
        return a.len().max(b.len());
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        current[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            current[j] = (prev[j] + 1)
                .min(current[j - 1] + 1)
                .min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut current);
    }
    prev[b.len()]
}

fn chars(s: &str) -> Vec<char> {
    s.chars().collect()
}

fn is_edit_whitespace(c: char) -> bool {
    c.is_whitespace()
}

fn trim_edit_whitespace(s: &str) -> &str {
    s.trim_matches(is_edit_whitespace)
}

type Replacer = fn(&str, &str) -> Vec<String>;

fn simple_replacer(_content: &str, find: &str) -> Vec<String> {
    vec![find.to_string()]
}

fn line_trimmed_replacer(content: &str, find: &str) -> Vec<String> {
    let original_lines: Vec<&str> = content.split('\n').collect();
    let mut search_lines: Vec<&str> = find.split('\n').collect();
    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }
    let mut out = Vec::new();
    if search_lines.is_empty() || search_lines.len() > original_lines.len() {
        return out;
    }
    for i in 0..=(original_lines.len() - search_lines.len()) {
        let matches = search_lines
            .iter()
            .enumerate()
            .all(|(j, s)| trim_edit_whitespace(original_lines[i + j]) == trim_edit_whitespace(s));
        if matches {
            out.push(original_lines[i..i + search_lines.len()].join("\n"));
        }
    }
    out
}

fn block_anchor_replacer(content: &str, find: &str) -> Vec<String> {
    let original_lines: Vec<&str> = content.split('\n').collect();
    let mut search_lines: Vec<&str> = find.split('\n').collect();
    if search_lines.len() < 3 {
        return Vec::new();
    }
    if search_lines.last() == Some(&"") {
        search_lines.pop();
    }
    let first_line_search = trim_edit_whitespace(search_lines[0]);
    let last_line_search = trim_edit_whitespace(search_lines[search_lines.len() - 1]);
    let search_block_size = search_lines.len();
    let max_line_delta = 1.max((search_block_size as f64 * 0.25).floor() as usize);

    let mut candidates: Vec<(usize, usize)> = Vec::new();
    for i in 0..original_lines.len() {
        if trim_edit_whitespace(original_lines[i]) != first_line_search {
            continue;
        }
        // The first matching last-anchor line at j ≥ i+2 decides.
        if let Some(j) = ((i + 2)..original_lines.len())
            .find(|&j| trim_edit_whitespace(original_lines[j]) == last_line_search)
        {
            let actual_block_size = j - i + 1;
            if actual_block_size.abs_diff(search_block_size) <= max_line_delta {
                candidates.push((i, j));
            }
        }
    }

    if candidates.is_empty() {
        return Vec::new();
    }

    let span_of =
        |start_line: usize, end_line: usize| original_lines[start_line..=end_line].join("\n");

    if candidates.len() == 1 {
        let (start_line, end_line) = candidates[0];
        let actual_block_size = end_line - start_line + 1;
        let mut similarity = 0.0f64;
        let lines_to_check = (search_block_size as i64 - 2).min(actual_block_size as i64 - 2);
        if lines_to_check > 0 {
            let mut j = 1;
            while j < search_block_size - 1 && j < actual_block_size - 1 {
                let original_line = trim_edit_whitespace(original_lines[start_line + j]);
                let search_line = trim_edit_whitespace(search_lines[j]);
                let (oc, sc) = (chars(original_line), chars(search_line));
                let max_len = oc.len().max(sc.len());
                if max_len > 0 {
                    similarity += (1.0 - levenshtein(&oc, &sc) as f64 / max_len as f64)
                        / lines_to_check as f64;
                    if similarity >= SINGLE_CANDIDATE_SIMILARITY_THRESHOLD {
                        break;
                    }
                }
                j += 1;
            }
        } else {
            similarity = 1.0;
        }
        if similarity >= SINGLE_CANDIDATE_SIMILARITY_THRESHOLD {
            return vec![span_of(start_line, end_line)];
        }
        return Vec::new();
    }

    let mut best_match: Option<(usize, usize)> = None;
    let mut max_similarity = -1.0f64;
    for &(start_line, end_line) in &candidates {
        let actual_block_size = end_line - start_line + 1;
        let mut similarity = 0.0f64;
        let lines_to_check = (search_block_size as i64 - 2).min(actual_block_size as i64 - 2);
        if lines_to_check > 0 {
            let mut j = 1;
            while j < search_block_size - 1 && j < actual_block_size - 1 {
                let original_line = trim_edit_whitespace(original_lines[start_line + j]);
                let search_line = trim_edit_whitespace(search_lines[j]);
                let (oc, sc) = (chars(original_line), chars(search_line));
                let max_len = oc.len().max(sc.len());
                if max_len > 0 {
                    similarity += 1.0 - levenshtein(&oc, &sc) as f64 / max_len as f64;
                }
                j += 1;
            }
            similarity /= lines_to_check as f64;
        } else {
            similarity = 1.0;
        }
        if similarity > max_similarity {
            max_similarity = similarity;
            best_match = Some((start_line, end_line));
        }
    }

    if max_similarity >= MULTIPLE_CANDIDATES_SIMILARITY_THRESHOLD
        && let Some((start_line, end_line)) = best_match
    {
        return vec![span_of(start_line, end_line)];
    }
    Vec::new()
}

fn normalize_whitespace(text: &str) -> String {
    let mut out = String::new();
    let mut in_ws = false;
    for c in text.chars() {
        if is_edit_whitespace(c) {
            in_ws = true;
        } else {
            if in_ws && !out.is_empty() {
                out.push(' ');
            }
            in_ws = false;
            out.push(c);
        }
    }
    out
}

/// Find the first substring of `line` where each word appears in order
/// separated by one-or-more Unicode whitespace characters. Returns the
/// exact matched span.
fn match_words_with_whitespace(line: &str, words: &[&str]) -> Option<String> {
    let line_chars = chars(line);
    'outer: for start in 0..=line_chars.len() {
        let mut pos = start;
        for (wi, word) in words.iter().enumerate() {
            if wi > 0 {
                let ws_start = pos;
                while pos < line_chars.len() && is_edit_whitespace(line_chars[pos]) {
                    pos += 1;
                }
                if pos == ws_start {
                    continue 'outer;
                }
            }
            let wc = chars(word);
            if line_chars.len() < pos + wc.len() || line_chars[pos..pos + wc.len()] != wc[..] {
                continue 'outer;
            }
            pos += wc.len();
        }
        return Some(line_chars[start..pos].iter().collect());
    }
    None
}

fn whitespace_normalized_replacer(content: &str, find: &str) -> Vec<String> {
    let normalized_find = normalize_whitespace(find);
    let mut out = Vec::new();

    let lines: Vec<&str> = content.split('\n').collect();
    for line in &lines {
        let norm_line = normalize_whitespace(line);
        if norm_line == normalized_find {
            out.push((*line).to_string());
        } else if norm_line.contains(&normalized_find) {
            let words: Vec<&str> = trim_edit_whitespace(find)
                .split(is_edit_whitespace)
                .filter(|w| !w.is_empty())
                .collect();
            if !words.is_empty()
                && let Some(m) = match_words_with_whitespace(line, &words)
            {
                out.push(m);
            }
        }
    }

    let find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.len() > 1 && find_lines.len() <= lines.len() {
        for i in 0..=(lines.len() - find_lines.len()) {
            let block = lines[i..i + find_lines.len()].join("\n");
            if normalize_whitespace(&block) == normalized_find {
                out.push(block);
            }
        }
    }
    out
}

fn remove_indentation(text: &str) -> String {
    let lines: Vec<&str> = text.split('\n').collect();
    let non_empty: Vec<&&str> = lines
        .iter()
        .filter(|line| !trim_edit_whitespace(line).is_empty())
        .collect();
    if non_empty.is_empty() {
        return text.to_string();
    }
    let min_indent = non_empty
        .iter()
        .map(|l| l.chars().take_while(|c| is_edit_whitespace(*c)).count())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| {
            if trim_edit_whitespace(l).is_empty() {
                (*l).to_string()
            } else {
                l.chars().skip(min_indent).collect()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn indentation_flexible_replacer(content: &str, find: &str) -> Vec<String> {
    let normalized_find = remove_indentation(find);
    let content_lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = find.split('\n').collect();
    let mut out = Vec::new();
    if find_lines.is_empty() || find_lines.len() > content_lines.len() {
        return out;
    }
    for i in 0..=(content_lines.len() - find_lines.len()) {
        let block = content_lines[i..i + find_lines.len()].join("\n");
        if remove_indentation(&block) == normalized_find {
            out.push(block);
        }
    }
    out
}

/// Normalize the common escaped characters used in model-authored source.
fn unescape_string(s: &str) -> String {
    let cs = chars(s);
    let mut out = String::new();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '\\' && i + 1 < cs.len() {
            let mapped = match cs[i + 1] {
                'n' => Some('\n'),
                't' => Some('\t'),
                'r' => Some('\r'),
                '\'' => Some('\''),
                '"' => Some('"'),
                '`' => Some('`'),
                '\\' => Some('\\'),
                '\n' => Some('\n'),
                '$' => Some('$'),
                _ => None,
            };
            if let Some(c) = mapped {
                out.push(c);
                i += 2;
                continue;
            }
        }
        out.push(cs[i]);
        i += 1;
    }
    out
}

fn escape_normalized_replacer(content: &str, find: &str) -> Vec<String> {
    let unescaped_find = unescape_string(find);
    let mut out = Vec::new();
    if content.contains(&unescaped_find) {
        out.push(unescaped_find.clone());
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = unescaped_find.split('\n').collect();
    if find_lines.is_empty() || find_lines.len() > lines.len() {
        return out;
    }
    for i in 0..=(lines.len() - find_lines.len()) {
        let block = lines[i..i + find_lines.len()].join("\n");
        if unescape_string(&block) == unescaped_find {
            out.push(block);
        }
    }
    out
}

fn trimmed_boundary_replacer(content: &str, find: &str) -> Vec<String> {
    let trimmed_find = trim_edit_whitespace(find);
    if trimmed_find == find {
        return Vec::new();
    }
    let mut out = Vec::new();
    if content.contains(trimmed_find) {
        out.push(trimmed_find.to_string());
    }
    let lines: Vec<&str> = content.split('\n').collect();
    let find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.is_empty() || find_lines.len() > lines.len() {
        return out;
    }
    for i in 0..=(lines.len() - find_lines.len()) {
        let block = lines[i..i + find_lines.len()].join("\n");
        if trim_edit_whitespace(&block) == trimmed_find {
            out.push(block);
        }
    }
    out
}

fn context_aware_replacer(content: &str, find: &str) -> Vec<String> {
    let mut find_lines: Vec<&str> = find.split('\n').collect();
    if find_lines.len() < 3 {
        return Vec::new();
    }
    if find_lines.last() == Some(&"") {
        find_lines.pop();
    }
    let content_lines: Vec<&str> = content.split('\n').collect();
    let first_line = trim_edit_whitespace(find_lines[0]);
    let last_line = trim_edit_whitespace(find_lines[find_lines.len() - 1]);
    let mut out = Vec::new();

    for i in 0..content_lines.len() {
        if trim_edit_whitespace(content_lines[i]) != first_line {
            continue;
        }
        for j in (i + 2)..content_lines.len() {
            if trim_edit_whitespace(content_lines[j]) == last_line {
                let block_lines = &content_lines[i..=j];
                if block_lines.len() == find_lines.len() {
                    let mut matching_lines = 0usize;
                    let mut total_non_empty = 0usize;
                    for k in 1..block_lines.len() - 1 {
                        let block_line = trim_edit_whitespace(block_lines[k]);
                        let find_line = trim_edit_whitespace(find_lines[k]);
                        if !block_line.is_empty() || !find_line.is_empty() {
                            total_non_empty += 1;
                            if block_line == find_line {
                                matching_lines += 1;
                            }
                        }
                    }
                    if total_non_empty == 0 || matching_lines as f64 / total_non_empty as f64 >= 0.5
                    {
                        out.push(block_lines.join("\n"));
                        break;
                    }
                }
                break;
            }
        }
    }
    out
}

fn multi_occurrence_replacer(content: &str, find: &str) -> Vec<String> {
    if find.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0;
    while let Some(idx) = content[start..].find(find) {
        out.push(find.to_string());
        start += idx + find.len();
    }
    out
}

/// The second anti-clobber guard for unexpectedly large fuzzy matches.
pub fn is_disproportionate_match(search: &str, old_string: &str) -> bool {
    let old_lines = old_string.split('\n').count();
    let search_lines = search.split('\n').count();
    if search_lines >= (old_lines + 3).max(old_lines * 2) {
        return true;
    }
    if old_lines == 1 {
        return false;
    }
    let search_trim = chars(trim_edit_whitespace(search)).len();
    let old_trim = chars(trim_edit_whitespace(old_string)).len();
    search_trim > (old_trim + 500).max(old_trim * 4)
}

const CASCADE: [Replacer; 9] = [
    simple_replacer,
    line_trimmed_replacer,
    block_anchor_replacer,
    whitespace_normalized_replacer,
    indentation_flexible_replacer,
    escape_normalized_replacer,
    trimmed_boundary_replacer,
    context_aware_replacer,
    multi_occurrence_replacer,
];

/// Try the replacement cascade in order; the first candidate span actually
/// present in `content` wins. `Err` carries stable model-facing error data.
pub fn fuzzy_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String, String> {
    if old_string == new_string {
        return Err("No changes to apply: old_string and new_string are identical.".to_string());
    }
    if old_string.is_empty() {
        return Err(EMPTY_OLD_STRING_MESSAGE.to_string());
    }

    let mut not_found = true;
    for replacer in CASCADE {
        for search in replacer(content, old_string) {
            let Some(index) = content.find(&search) else {
                continue;
            };
            not_found = false;
            if is_disproportionate_match(&search, old_string) {
                return Err(
                    "Refusing replacement because the matched span is much larger than old_string. Re-read the file and provide the full exact old_string for the intended replacement."
                        .to_string(),
                );
            }
            if replace_all {
                return Ok(content.replace(&search, new_string));
            }
            let last_index = content.rfind(&search).unwrap_or(index);
            if index != last_index {
                continue;
            }
            return Ok(format!(
                "{}{}{}",
                &content[..index],
                new_string,
                &content[index + search.len()..]
            ));
        }
    }

    if not_found {
        return Err(
            "Could not find old_string in the file. It must match exactly, including whitespace, indentation, and line endings."
                .to_string(),
        );
    }
    Err(
        "Found multiple matches for old_string. Provide more surrounding context to make the match unique, or set replace_all."
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerant_match_and_ambiguity_guards() {
        assert_eq!(
            fuzzy_replace(
                "fn x() {\n    value();\n}",
                "fn x() {\n value();\n}",
                "gone",
                false
            )
            .unwrap(),
            "gone"
        );
        assert!(fuzzy_replace("dup dup", "dup", "x", false).is_err());
        assert_eq!(fuzzy_replace("dup dup", "dup", "x", true).unwrap(), "x x");
        assert!(fuzzy_replace("same", "same", "same", false).is_err());
        assert!(fuzzy_replace("content", "", "x", false).is_err());
    }

    #[test]
    fn line_endings_round_trip() {
        let source = "a\r\nb";
        let ending = detect_line_ending(source);
        assert_eq!(ending, "\r\n");
        assert_eq!(
            convert_to_line_ending(&normalize_line_endings("x\ny"), ending),
            "x\r\ny"
        );
    }
}
