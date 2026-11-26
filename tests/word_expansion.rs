use std::process::Command;

fn assert_expansion(source: &str, args: &[&str], expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let script = format!(
        "show() {{ printf 'argc=%s\\n' \"$#\"; for arg; do printf '<%s>\\n' \"$arg\"; done; }}\n{source}"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("IFS", ":") // An invoked shell must initialize its own IFS (2.5.3).
        .args(["-c", &script, "expansion-test"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    assert!(output.stderr.is_empty(), "{:?}", output);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
}

// POSIX.1-2024 Shell Command Language 2.6.5.
#[test]
fn default_and_unset_ifs_split_only_unquoted_expansions() {
    assert_expansion(
        r#"value='  a	b
c  '
show $value
unset IFS
show ${value} "$value" $(printf '  d\te\nf\n\n')
show left${value}right left"$value"right
"#,
        &[],
        "argc=3\n<a>\n<b>\n<c>\nargc=7\n<a>\n<b>\n<c>\n<  a\tb\nc  >\n<d>\n<e>\n<f>\nargc=6\n<left>\n<a>\n<b>\n<c>\n<right>\n<left  a\tb\nc  right>\n",
    );
}

#[test]
fn custom_ifs_distinguishes_whitespace_and_non_whitespace_terminators() {
    assert_expansion(
        r#"IFS=' :'
value='  : a  : :b:  '
show $value
show literal:colon a\ b \: $value/tail
left='a '; right=': b'; show $left$right
unset missing; show ${missing:-a:b} ${missing:-$left$right}
"#,
        &[],
        "argc=4\n<>\n<a>\n<>\n<b>\nargc=8\n<literal:colon>\n<a b>\n<:>\n<>\n<a>\n<>\n<b>\n</tail>\nargc=2\n<a>\n<b>\nargc=4\n<a>\n<b>\n<a>\n<b>\n",
    );
}

#[test]
fn null_ifs_still_drops_implicit_empty_fields_but_not_quoted_ones() {
    assert_expansion(
        r#"unset missing; empty=; spaces='   '
show $missing $empty $spaces $(printf '')
show "$missing" "$empty" '' $empty"" pre${empty}post
IFS=
show $missing $empty $spaces $(printf '')
show "$missing" "$empty" '' $empty"" pre${empty}post
"#,
        &[],
        "argc=0\nargc=5\n<>\n<>\n<>\n<>\n<prepost>\nargc=1\n<   >\nargc=5\n<>\n<>\n<>\n<>\n<prepost>\n",
    );
}

// Sections 2.7, 2.7.4 and 2.9.1.1: these scalar contexts do not split.
#[test]
fn assignments_redirection_operands_and_here_documents_do_not_split() {
    assert_expansion(
        r#"IFS=' :'
value='a b:c'
saved=$value
export other=$(printf 'a b:c')
show "$saved" "$other" plain=$value
printf 'from file\n' >$value
/bin/cat <$(printf 'a b:c') | /bin/cat
/bin/cat <<EOF
$value
$(printf 'a b:c')
EOF
"#,
        &[],
        "argc=5\n<a b:c>\n<a b:c>\n<plain=a>\n<b>\n<c>\nfrom file\na b:c\na b:c\n",
    );
}

#[test]
fn pipeline_stages_and_explicit_for_lists_split_expansions() {
    assert_expansion(
        r#"IFS=':'; value='x y::z'
printf '<%s>\n' $value | /bin/cat
for item in $value; do show "$item"; done
"#,
        &[],
        "<x y>\n<>\n<z>\nargc=1\n<x y>\nargc=1\n<>\nargc=1\n<z>\n",
    );
}
