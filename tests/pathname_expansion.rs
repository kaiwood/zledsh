use std::{fs, process::Command};

fn assert_paths(files: &[&str], source: &str, expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&work).unwrap();
    for name in files {
        let path = work.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "file contents\n").unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(work)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .args(["-c", source])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
}

// POSIX.1-2024 Shell Command Language 2.6.6 and 2.14.3.
#[test]
fn basic_patterns_expand_and_unmatched_patterns_are_unchanged() {
    assert_paths(
        &["a space.txt", "b.txt", "c.log"],
        r#"printf '<%s>\n' a*.txt ?.txt *.missing"#,
        "<a space.txt>\n<b.txt>\n<*.missing>\n",
    );
}

#[test]
fn only_unquoted_metacharacters_are_active() {
    assert_paths(
        &["x*1.txt", "x?2.txt", "xy3.txt"],
        r#"pattern='xy?.txt'
printf '<%s>\n' 'x*.txt' x\?.txt x"*"?.txt x\??.txt "$pattern" $pattern $(printf 'xy*.txt')
"#,
        "<x*.txt>\n<x?.txt>\n<x*1.txt>\n<x?2.txt>\n<xy?.txt>\n<xy3.txt>\n<xy3.txt>\n",
    );
}

#[test]
fn bracket_expressions_match_lists_ranges_negation_and_classes() {
    assert_paths(
        &["x1", "x2", "x3", "xa", "xb", "x-", "x]", "x[1]"],
        r#"printf '<%s>\n' x[13] x[1-3] x[!2ab] x[[:digit:]] x[1'-'3] x[]] x\[1\] x[unterminated
"#,
        "<x1>\n<x3>\n<x1>\n<x2>\n<x3>\n<x->\n<x1>\n<x3>\n<x]>\n<x1>\n<x2>\n<x3>\n<x->\n<x1>\n<x3>\n<x]>\n<x[1]>\n<x[unterminated>\n",
    );
}

#[test]
fn pathname_components_and_explicit_leading_dots() {
    assert_paths(
        &[
            "tree/one/a.txt",
            "tree/one/.hidden.txt",
            "tree/two/b.txt",
            "tree/.secret/c.txt",
            "tree/plain.txt",
            "m[n/o]p.txt",
        ],
        r#"printf '<%s>\n' tree/*.txt tree/*/?.txt tree/.*/*.txt tree/*/"."*.txt m[n/o]p.*
"#,
        "<tree/plain.txt>\n<tree/one/a.txt>\n<tree/two/b.txt>\n<tree/.secret/c.txt>\n<tree/one/.hidden.txt>\n<m[n/o]p.txt>\n",
    );
}

#[test]
fn argument_and_for_list_patterns_expand_but_redirections_and_assignments_do_not() {
    assert_paths(
        &["report-1.txt", "report-2.txt", "report-?.txt"],
        r#"saved=report-?.txt
export other=report-[12].txt
printf '<%s>\n' "$saved" "$other"
for entry in report-[12].txt; do printf '<%s>\n' "$entry"; done
printf 'only the literal file\n' >report-?.txt
/bin/cat <$(printf 'report-?.txt') | /bin/cat
/bin/cat <report-1.txt
/bin/cat <<EOF
report-[12].txt
$saved
EOF
"#,
        "<report-?.txt>\n<report-[12].txt>\n<report-1.txt>\n<report-2.txt>\nonly the literal file\nfile contents\nreport-[12].txt\nreport-?.txt\n",
    );
}

#[test]
fn matches_are_sorted_per_pattern_in_c_locale() {
    assert_paths(
        &[
            "order-zoo",
            "order-apple",
            "order-Zebra",
            "order-2",
            "a/end",
            "a-/end",
        ],
        r#"printf '<%s>\n' order-z* order-* a*//end"#,
        "<order-zoo>\n<order-2>\n<order-Zebra>\n<order-apple>\n<order-zoo>\n<a-//end>\n<a//end>\n",
    );
}

#[test]
fn globbing_follows_splitting_but_never_resplits_or_reglobs_the_matches() {
    assert_paths(
        &["item one.txt", "item*.txt", "item2.txt", "next space.log"],
        r#"patterns='item*.txt:next*.log'; IFS=' :'
show() { printf 'argc=%s\n' "$#"; for arg; do printf '<%s>\n' "$arg"; done; }
show $patterns "$patterns" item"*".txt
"#,
        "argc=6\n<item one.txt>\n<item*.txt>\n<item2.txt>\n<next space.log>\n<item*.txt:next*.log>\n<item*.txt>\n",
    );
}

#[test]
fn backslashes_from_unquoted_expansions_can_escape_pattern_characters() {
    assert_paths(
        &["q*1.txt", "q21.txt", "r\\1.txt"],
        r#"match='q\*?.txt'; missing='q\*?.missing'; inactive='q\*1.txt'
printf '<%s>\n' $match $missing $inactive 'r\'?.txt
"#,
        "<q*1.txt>\n<q\\*?.missing>\n<q\\*1.txt>\n<r\\1.txt>\n",
    );
}

#[test]
fn noglob_preserves_field_splitting_and_does_not_disable_case_patterns() {
    assert_paths(
        &["a.txt", "b.txt"],
        r#"
set -f -- keep
words='*.txt extra'
printf '<%s>\n' "$-" "$1" $words
for value in *.txt; do printf '<%s>\n' "$value"; done
case a.txt in *.txt) printf 'matched\n';; esac
( set +o noglob; printf '<%s>\n' *.txt )
printf '<%s>\n' *.txt
true | eval 'printf "<%s>\n" *.txt'
set +f
printf '<%s>\n' *.txt
"#,
        "<f>\n<keep>\n<*.txt>\n<extra>\n<*.txt>\nmatched\n<a.txt>\n<b.txt>\n<*.txt>\n<*.txt>\n<a.txt>\n<b.txt>\n",
    );
}

#[test]
fn noglob_can_be_enabled_at_invocation() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .env_clear()
        .env("HOME", dir.path())
        .args(["-fc", "printf '<%s>\\n' \"$-\" *.txt"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(output.stdout, b"<f>\n<*.txt>\n");
}

#[test]
fn opt_in_globstar_matches_zero_or_more_visible_directory_levels() {
    assert_paths(
        &[
            "top.txt",
            "a/next.txt",
            "a/b/deep.txt",
            ".hidden/secret.txt",
        ],
        r#"
printf '<%s>\n' **/*.txt
set -o globstar
printf '<%s>\n' **/*.txt
printf '<%s>\n' a/**/
printf '<%s>\n' '**'/*.txt
set -f
printf '<%s>\n' **/*.txt
"#,
        "<a/next.txt>\n<a/b/deep.txt>\n<a/next.txt>\n<top.txt>\n<a/>\n<a/b/>\n<**/*.txt>\n<**/*.txt>\n",
    );
}
