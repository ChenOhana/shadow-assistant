//! Line diffs for tier-2 session text: the lines a replay made new
//! ([`changed_lines`]), and a git-style unified diff of a saved file
//! ([`unified_diff`]).

/// Upper bound on the edit distance the Myers search explores. Past it, the
/// whole differing middle (after trimming the common prefix and suffix) counts
/// as changed. The bound keeps the trace at a few MiB, inside `brain run`'s
/// memory budget, however large the file is.
const MAX_EDIT_DISTANCE: usize = 512;

/// One step of a line diff, as indices into the old (`a`) and new (`b`) lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

/// The line diff of `a` into `b`: equal lines, deletions and insertions in
/// file order, deletions before insertions where both happen.
fn line_ops(a: &[&str], b: &[&str]) -> Vec<Op> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let a_mid = &a[prefix..a.len() - suffix];
    let b_mid = &b[prefix..b.len() - suffix];
    let (deleted, inserted) =
        myers(a_mid, b_mid).unwrap_or_else(|| (vec![true; a_mid.len()], vec![true; b_mid.len()]));

    let mut ops: Vec<Op> = (0..prefix).map(|i| Op::Equal(i, i)).collect();
    let (mut i, mut j) = (0, 0);
    while i < a_mid.len() || j < b_mid.len() {
        if i < a_mid.len() && deleted[i] {
            ops.push(Op::Delete(prefix + i));
            i += 1;
        } else if j < b_mid.len() && inserted[j] {
            ops.push(Op::Insert(prefix + j));
            j += 1;
        } else {
            ops.push(Op::Equal(prefix + i, prefix + j));
            i += 1;
            j += 1;
        }
    }
    let (a_end, b_end) = (a.len() - suffix, b.len() - suffix);
    ops.extend((0..suffix).map(|k| Op::Equal(a_end + k, b_end + k)));
    slide_down(&mut ops, a, b);
    ops
}

/// Moves each run of only insertions (or only deletions) as far down as
/// possible while the text stays the same: when the line after the run
/// equals the run's first line, the run can start one line later. This is
/// how git places ambiguous changes, e.g. a function added after an
/// existing `}` shows as added after it, not before it.
fn slide_down(ops: &mut [Op], a: &[&str], b: &[&str]) {
    let mut start = 0;
    while start < ops.len() {
        let kind = |op: &Op| match op {
            Op::Insert(_) => 1,
            Op::Delete(_) => 2,
            Op::Equal(..) => 0,
        };
        let k = kind(&ops[start]);
        let mut end = start;
        while end < ops.len() && kind(&ops[end]) == k {
            end += 1;
        }
        // A pure run (an equal line, or the start, before it) slides while
        // the equal line after it has the same text as the run's first line.
        let pure = k != 0 && (start == 0 || kind(&ops[start - 1]) == 0);
        while pure && end < ops.len() {
            let Op::Equal(i, j) = ops[end] else { break };
            let len = end - start;
            match ops[start] {
                // Insert(j0..j0+len) then Equal(i, j0+len) becomes
                // Equal(i, j0) then Insert(j0+1..=j0+len).
                Op::Insert(j0) if b[j0] == b[j] => {
                    ops[start] = Op::Equal(i, j0);
                    for n in 1..=len {
                        ops[start + n] = Op::Insert(j0 + n);
                    }
                }
                // Delete(i0..i0+len) then Equal(i0+len, j) becomes
                // Equal(i0, j) then Delete(i0+1..=i0+len).
                Op::Delete(i0) if a[i0] == a[i] => {
                    ops[start] = Op::Equal(i0, j);
                    for n in 1..=len {
                        ops[start + n] = Op::Delete(i0 + n);
                    }
                }
                _ => break,
            }
            start += 1;
            end += 1;
        }
        start = end.max(start + 1);
    }
}

/// The lines of `after` that are new or modified relative to `before`
/// (line-level diff, new side only), in file order, joined with `'\n'`.
/// Empty if nothing changed.
pub fn changed_lines(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    line_ops(&a, &b)
        .into_iter()
        .filter_map(|op| match op {
            Op::Insert(j) => Some(b[j]),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A git-style unified diff of `before` into `after`, with `context`
/// unchanged lines around each change (git's default is 3): `--- a/<name>`
/// and `+++ b/<name>` headers, then hunks `@@ -l,s +l,s @@` whose lines
/// start with ` ` (unchanged), `-` (removed) or `+` (added). Empty if
/// nothing changed. Line endings are not compared.
pub fn unified_diff(before: &str, after: &str, name: &str, context: usize) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let ops = line_ops(&a, &b);
    let changes: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| !matches!(op, Op::Equal(..)))
        .map(|(k, _)| k)
        .collect();
    if changes.is_empty() {
        return String::new();
    }
    // Group changes whose surrounding context would touch or overlap.
    let mut hunks: Vec<(usize, usize)> = Vec::new(); // op index ranges, inclusive
    for &k in &changes {
        let lo = k.saturating_sub(context);
        let hi = (k + context).min(ops.len() - 1);
        match hunks.last_mut() {
            Some((_, end)) if lo <= *end + 1 => *end = hi,
            _ => hunks.push((lo, hi)),
        }
    }
    let mut out = format!("--- a/{name}\n+++ b/{name}\n");
    for (lo, hi) in hunks {
        let slice = &ops[lo..=hi];
        // Line numbers (1-based) where the hunk starts on each side.
        let a_start = ops[..lo]
            .iter()
            .filter(|op| !matches!(op, Op::Insert(_)))
            .count();
        let b_start = ops[..lo]
            .iter()
            .filter(|op| !matches!(op, Op::Delete(_)))
            .count();
        let a_len = slice
            .iter()
            .filter(|op| !matches!(op, Op::Insert(_)))
            .count();
        let b_len = slice
            .iter()
            .filter(|op| !matches!(op, Op::Delete(_)))
            .count();
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            hunk_range(a_start, a_len),
            hunk_range(b_start, b_len)
        ));
        for op in slice {
            let line = match *op {
                Op::Equal(i, _) => format!(" {}", a[i]),
                Op::Delete(i) => format!("-{}", a[i]),
                Op::Insert(j) => format!("+{}", b[j]),
            };
            out.push_str(&line);
            out.push('\n');
        }
    }
    out.truncate(out.trim_end_matches('\n').len());
    out
}

/// A hunk range as git writes it: `start,len`, `start` alone when the length
/// is 1, and for an empty range the line before it (`0` at the very start).
fn hunk_range(lines_before: usize, len: usize) -> String {
    match len {
        0 => format!("{lines_before},0"),
        1 => format!("{}", lines_before + 1),
        n => format!("{},{n}", lines_before + 1),
    }
}

/// Myers' O((N+M)·D) diff. Returns, for each line of `a`, whether it is
/// deleted, and for each line of `b`, whether it is inserted (lines not part
/// of the longest common subsequence), or `None` if the edit distance exceeds
/// [`MAX_EDIT_DISTANCE`].
fn myers(a: &[&str], b: &[&str]) -> Option<(Vec<bool>, Vec<bool>)> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    if n == 0 || m == 0 {
        return Some((vec![true; a.len()], vec![true; b.len()]));
    }
    let max_d = ((n + m) as usize).min(MAX_EDIT_DISTANCE) as isize;
    let offset = max_d + 1;
    let width = (2 * max_d + 3) as usize;
    let mut v = vec![0isize; width];
    let mut trace: Vec<Vec<isize>> = Vec::new();
    let mut found = None;
    'outer: for d in 0..=max_d {
        let mut k = -d;
        while k <= d {
            let idx = (k + offset) as usize;
            let mut x = if k == -d || (k != d && v[idx - 1] < v[idx + 1]) {
                v[idx + 1]
            } else {
                v[idx - 1] + 1
            };
            let mut y = x - k;
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[idx] = x;
            if x >= n && y >= m {
                trace.push(v.clone());
                found = Some(d);
                break 'outer;
            }
            k += 2;
        }
        trace.push(v.clone());
    }
    let d_final = found?;

    // Backtrack through the trace: each step down is an insertion of a line
    // of `b`, each step right a deletion of a line of `a`.
    let mut deleted = vec![false; a.len()];
    let mut inserted = vec![false; b.len()];
    let (mut x, mut y) = (n, m);
    for d in (1..=d_final).rev() {
        let v = &trace[(d - 1) as usize];
        let k = x - y;
        let prev_k =
            if k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]) {
                k + 1
            } else {
                k - 1
            };
        let prev_x = v[(prev_k + offset) as usize];
        let prev_y = prev_x - prev_k;
        while x > prev_x && y > prev_y {
            x -= 1;
            y -= 1;
        }
        if prev_k == k + 1 {
            inserted[prev_y as usize] = true;
        } else {
            deleted[prev_x as usize] = true;
        }
        x = prev_x;
        y = prev_y;
    }
    Some((deleted, inserted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_is_empty() {
        assert_eq!(changed_lines("a\nb\n", "a\nb\n"), "");
        assert_eq!(changed_lines("", ""), "");
    }

    #[test]
    fn appended_and_inserted_lines() {
        assert_eq!(changed_lines("a\nb\n", "a\nb\nc\n"), "c");
        assert_eq!(changed_lines("a\nc\n", "a\nb\nc\n"), "b");
        assert_eq!(changed_lines("", "x\ny"), "x\ny");
    }

    #[test]
    fn modified_lines_show_new_side_only() {
        assert_eq!(
            changed_lines("fn a() {}\nfn b() {}\n", "fn a() { 1 }\nfn b() {}\n"),
            "fn a() { 1 }"
        );
        assert_eq!(changed_lines("a\nb\nc\n", "a\nc\n"), "");
    }

    #[test]
    fn scattered_changes_in_file_order() {
        let before = "1\n2\n3\n4\n5\n6\n";
        let after = "0\n1\n2\nthree\n4\n5\n6\n7\n";
        assert_eq!(changed_lines(before, after), "0\nthree\n7");
    }

    #[test]
    fn repeated_lines() {
        assert_eq!(changed_lines("x\nx\n", "x\nx\nx\n"), "x");
        assert_eq!(changed_lines("}\n}\n", "}\n  y\n}\n"), "  y");
    }

    #[test]
    fn unified_diff_like_git() {
        let before = "// header\n\n#include <stdio>\n\nint f() {\n    return 1;\n}\n";
        let after = "// header\n\n#include <iostream>\n\nint f() {\n    return 1;\n}\n\nint main() {\n    return f();\n}\n";
        assert_eq!(
            unified_diff(before, after, "test.cpp", 3),
            "--- a/test.cpp\n+++ b/test.cpp\n\
             @@ -1,7 +1,11 @@\n \
             // header\n \n-#include <stdio>\n+#include <iostream>\n \n \
             int f() {\n     return 1;\n }\n+\n+int main() {\n+    return f();\n+}"
        );
        assert_eq!(unified_diff("a\nb\n", "a\nb\n", "x", 3), "");
    }

    #[test]
    fn unified_diff_hunks_and_ranges() {
        // Changes more than 2×3 lines apart get separate hunks.
        let before: String = (1..=20).map(|i| format!("{i}\n")).collect();
        let after: String = (1..=20)
            .filter(|&i| i != 18)
            .map(|i| {
                if i == 2 {
                    "two\n".to_string()
                } else {
                    format!("{i}\n")
                }
            })
            .collect();
        assert_eq!(
            unified_diff(&before, &after, "n", 3),
            "--- a/n\n+++ b/n\n\
             @@ -1,5 +1,5 @@\n 1\n-2\n+two\n 3\n 4\n 5\n\
             @@ -15,6 +15,5 @@\n 15\n 16\n 17\n-18\n 19\n 20"
        );
        // An empty side is written as "start,0", a single line without ",1".
        assert_eq!(
            unified_diff("", "x\n", "n", 3),
            "--- a/n\n+++ b/n\n@@ -0,0 +1 @@\n+x"
        );
        assert_eq!(
            unified_diff("x\n", "", "n", 3),
            "--- a/n\n+++ b/n\n@@ -1 +0,0 @@\n-x"
        );
        // No context: only the changed lines.
        assert_eq!(
            unified_diff("a\nb\nc\n", "a\nB\nc\n", "n", 0),
            "--- a/n\n+++ b/n\n@@ -2 +2 @@\n-b\n+B"
        );
    }

    #[test]
    fn huge_distance_falls_back_to_whole_middle() {
        let before: String = (0..2000).map(|i| format!("a{i}\n")).collect();
        let after: String = (0..2000).map(|i| format!("b{i}\n")).collect();
        let out = changed_lines(&before, &after);
        assert_eq!(out.lines().count(), 2000);
        assert!(out.starts_with("b0\nb1"));
    }
}
