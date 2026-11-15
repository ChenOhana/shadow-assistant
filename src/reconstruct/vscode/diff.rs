//! Line diff for tier-2 session text: which lines of the saved file are new.

/// Upper bound on the edit distance the Myers search explores. Past it, the
/// whole differing middle (after trimming the common prefix and suffix) counts
/// as changed. The bound keeps the trace at a few MiB, inside `brain run`'s
/// memory budget, however large the file is.
const MAX_EDIT_DISTANCE: usize = 512;

/// The lines of `after` that are new or modified relative to `before`
/// (line-level diff, new side only), in file order, joined with `'\n'`.
/// Empty if nothing changed.
pub fn changed_lines(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();

    let prefix = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let a_mid = &a[prefix..a.len() - suffix];
    let b_mid = &b[prefix..b.len() - suffix];

    let new_in_mid: Vec<bool> = match myers_inserted(a_mid, b_mid) {
        Some(flags) => flags,
        None => vec![true; b_mid.len()],
    };
    b_mid
        .iter()
        .zip(new_in_mid)
        .filter(|(_, new)| *new)
        .map(|(l, _)| *l)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Myers' O((N+M)·D) diff. Returns, for each line of `b`, whether it is an
/// insertion (not part of the longest common subsequence), or `None` if the
/// edit distance exceeds [`MAX_EDIT_DISTANCE`].
fn myers_inserted(a: &[&str], b: &[&str]) -> Option<Vec<bool>> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    if n == 0 {
        return Some(vec![true; b.len()]);
    }
    if m == 0 {
        return Some(Vec::new());
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

    // Backtrack through the trace to mark which lines of `b` were inserted.
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
            // Moved down: an insertion of b[prev_y].
            inserted[prev_y as usize] = true;
        }
        x = prev_x;
        y = prev_y;
    }
    Some(inserted)
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
    fn huge_distance_falls_back_to_whole_middle() {
        let before: String = (0..2000).map(|i| format!("a{i}\n")).collect();
        let after: String = (0..2000).map(|i| format!("b{i}\n")).collect();
        let out = changed_lines(&before, &after);
        assert_eq!(out.lines().count(), 2000);
        assert!(out.starts_with("b0\nb1"));
    }
}
