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

// POSIX.1-2024 Shell Command Language 2.5.2: separate fields and edge attachment.
#[test]
fn quoted_at_preserves_boundaries_and_unquoted_at_splits_each_parameter() {
    assert_expansion(
        r#"show "$@"
show "pre${@}post"
show $@
show pre${@}post
IFS=
show $@
show pre${@}post
"#,
        &["", "a b", ""],
        "argc=3\n<>\n<a b>\n<>\nargc=3\n<pre>\n<a b>\n<post>\nargc=2\n<a>\n<b>\nargc=4\n<pre>\n<a>\n<b>\n<post>\nargc=1\n<a b>\nargc=3\n<pre>\n<a b>\n<post>\n",
    );
}

#[test]
fn star_joins_without_splitting_but_preserves_initial_fields_when_unquoted() {
    assert_expansion(
        r#"unset IFS
show "$*"
show $*
IFS=': '
show "pre${*}post"
show pre${*}post
saved=$*
show "$saved"
IFS=
show "$*"
show $*
"#,
        &["a b", "", "c:d"],
        "argc=1\n<a b  c:d>\nargc=3\n<a>\n<b>\n<c:d>\nargc=1\n<prea b::c:dpost>\nargc=4\n<prea>\n<b>\n<c>\n<dpost>\nargc=1\n<a b::c:d>\nargc=1\n<a bc:d>\nargc=2\n<a b>\n<c:d>\n",
    );
}

#[test]
fn zero_parameters_do_not_create_at_fields_or_remove_adjacent_text_or_quotes() {
    assert_expansion(
        r#"show "$@" $@ $*
show "$*" "pre$@post" pre${@}post "pre${*}post" pre${*}post
show "$@"'' ''$@
unset missing; show "${missing:-$@}" "${missing:-}"
IFS=; show "$@" $@ "$*" $*
"#,
        &[],
        "argc=0\nargc=5\n<>\n<prepost>\n<prepost>\n<prepost>\n<prepost>\nargc=2\n<>\n<>\nargc=1\n<>\nargc=1\n<>\n",
    );
}

// POSIX.1-2024 2.6.1: assignment tildes are recognized after '=' and ':'.
#[test]
fn tilde_expands_only_in_eligible_unquoted_positions() {
    assert_expansion(
        r#"HOME='/with space/'
x=~:~/bin:pre~:~no_such_zledsh_test_login/path:'~':\~
export y=~/lib:~
show "$x" "$y" ~/bin '~no_such_zledsh_test_login' ~no_such_zledsh_test_login ordinary=~/bin "~" \~/bin
"#,
        &[],
        "argc=8\n</with space/:/with space/bin:pre~:~no_such_zledsh_test_login/path:~:~>\n</with space/lib:/with space/>\n</with space/bin>\n<~no_such_zledsh_test_login>\n<~no_such_zledsh_test_login>\n<ordinary=~/bin>\n<~>\n<~/bin>\n",
    );
}

#[test]
fn named_tilde_looks_up_home_independently_of_shell_home() {
    // SAFETY: root is a terminated login string; this integration test is the
    // only caller of getpwnam in this process and copies the result immediately.
    let home = unsafe {
        let entry = libc::getpwnam(c"root".as_ptr());
        assert!(!entry.is_null(), "test platform has no root account");
        std::ffi::CStr::from_ptr((*entry).pw_dir)
            .to_str()
            .unwrap()
            .to_owned()
    };
    let prefix = home.strip_suffix('/').unwrap_or(&home);
    assert_expansion(
        r#"HOME=/not-root
value=~root/lib:~root
show ~root ~root/bin "$value" "~root/bin"
"#,
        &[],
        &format!("argc=4\n<{home}>\n<{prefix}/bin>\n<{prefix}/lib:{home}>\n<~root/bin>\n"),
    );
}

// POSIX.1-2024 2.6.2: test null only with the colon, and expand words lazily.
#[test]
fn parameter_length_assignment_and_quoted_operator_words() {
    assert_expansion(
        r#"HOME=/home-fixture
unset missing untouched never
empty=
show "${#missing}" "${#empty}" "${empty=not assigned}" "${empty:=filled}" "$empty"
show ${missing='a b'} "$missing" "${#missing}" "${missing:=ignored}"
show ${untouched:-"a } b"} ${untouched:-a\}b} ${untouched:-''} ${untouched:-~/bin}
show "${missing?${never=bad}}" "${#never}"
"#,
        &[],
        "argc=5\n<0>\n<0>\n<>\n<filled>\n<filled>\nargc=5\n<a>\n<b>\n<a b>\n<3>\n<a b>\nargc=4\n<a } b>\n<a}b>\n<>\n</home-fixture/bin>\nargc=2\n<a b>\n<0>\n",
    );
}

#[test]
fn parameter_error_terminates_noninteractive_execution() {
    let dir = tempfile::tempdir().unwrap();
    for command in [
        "printf '%s' ${value:?\"required value\"}",
        "printf wrong >${value:?\"required value\"}",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
            .current_dir(dir.path()).env_clear().env("HOME", dir.path()).env("PATH", "/usr/bin:/bin")
            .args(["-c", &format!("value=; printf '<%s>\\n' \"${{value?unused}}\"; {command}; printf wrong > continued")])
            .output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert_eq!(output.stdout, b"<>\n");
        assert_eq!(output.stderr, b"zledsh: value: required value\n");
        assert!(!dir.path().join("continued").exists());
    }
}

#[test]
fn parameter_trimming_uses_shortest_or_longest_text_match() {
    assert_expansion(
        r#"value='.a/b/c.txt.txt'; slash='*/'; tail='.*'
show "${value#$slash}" "${value##$slash}" "${value%$tail}" "${value%%$tail}"
show "${value#nomatch}" "${value#*}" "${value##*}" "${value%*}" "${value%%*}"
value='*a*b'; star='*'
show "${value#"$star"}" "${value#\*a}" "${value%"*b"}" "${value##[!*]}"
unset value; show "${value#*}" ${value%*}
"#,
        &[],
        "argc=4\n<b/c.txt.txt>\n<c.txt.txt>\n<.a/b/c.txt>\n<>\nargc=5\n<.a/b/c.txt.txt>\n<.a/b/c.txt.txt>\n<>\n<.a/b/c.txt.txt>\n<>\nargc=4\n<a*b>\n<*b>\n<*a>\n<*a*b>\nargc=1\n<>\n",
    );
}

// POSIX.1-2024 2.2.4: escaped bytes are protected from splitting and globbing.
#[test]
fn dollar_single_quotes_preserve_escaped_text_as_one_argument() {
    assert_expansion(
        r#"show $'one\ttwo\nthree' pre$''post $'' $'\101\x42\xC3\xA9' $'\'\"\\*?' $'\cA\c?\e' $'keep\000discard'"#,
        &[],
        "argc=7\n<one\ttwo\nthree>\n<prepost>\n<>\n<ABé>\n<'\"\\*?>\n<\x01\x7f\x1b>\n<keep>\n",
    );
    assert_expansion(
        r#"unset value
show ${value:-$'a\tb'} "$'literal'"
/bin/cat <<$'E\x4eD'
$value
END
"#,
        &[],
        "argc=2\n<a\tb>\n<$'literal'>\n$value\n",
    );
}

// POSIX.1-2024 2.6.3: both substitution forms execute native shell code.
#[test]
fn backquoted_substitution_preserves_status_quoting_and_nested_documents() {
    assert_expansion(
        r#"result=`printf 'a b\n\n'; exit 7`
show "$?" "$result" `printf 'a b'` "`printf 'a b'`"
show `printf '<%s>' \`printf nested\``
/bin/cat <<END
`printf 'document value'`
\`literal\`
END
show "`/bin/cat <<'BODY'
quoted $value
BODY
`"
"#,
        &[],
        "argc=5\n<7>\n<a b>\n<a>\n<b>\n<a b>\nargc=1\n<<nested>>\ndocument value\n`literal`\nargc=1\n<quoted $value>\n",
    );
}

// POSIX.1-2024 2.6.4: integer expressions retain assignment side effects.
#[test]
fn arithmetic_expands_operands_and_short_circuits_without_side_effects() {
    assert_expansion(
        r#"n=-2; offset=2
show "$((n * 3 + 010 + 0xA))" "$((($offset + 1) << 2))" "$((n))" "$(( $n ))"
show "$((a = 4, a += 3))" "$a" "$((a > 5 && a < 9))"
show "$((0 && (a = 90)))" "$((1 || (1 / 0)))" "$((0 ? (a = 80) : 6))" "$a"
show "$(( $(printf 9) + $((1 + 2)) ))"
/bin/cat <<END
sum=$((a + 1))
END
"#,
        &[],
        "argc=4\n<12>\n<12>\n<-2>\n<-2>\nargc=3\n<7>\n<7>\n<1>\nargc=4\n<0>\n<1>\n<6>\n<7>\nargc=1\n<12>\nsum=8\n",
    );
}

#[test]
fn arithmetic_reports_invalid_variables_and_division() {
    for (script, message) in [
        (
            "printf '%s' $((1 / 0)); printf wrong",
            "invalid arithmetic division",
        ),
        (
            "n=bad; printf '%s' $((n + 1)); printf wrong",
            "arithmetic variable is not an integer",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
            .args(["-c", script])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!("zledsh: arithmetic expansion: {message}\n")
        );
    }
}

#[test]
fn brace_expansion_is_lexical_and_preserves_literal_braces() {
    let home = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(home.path())
        .env_clear()
        .env("HOME", home.path())
        .args([
            "-c",
            r#"
value='{x,y}'
printf '<%s>\n' pre{a,{b,c}}{1,2} '{q,r}' \{e,f\} {literal} "$value" {,z}
printf '<%s>\n' {03..01..2} {x..z}
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"<prea1>\n<prea2>\n<preb1>\n<preb2>\n<prec1>\n<prec2>\n<{q,r}>\n<{e,f}>\n<{literal}>\n<{x,y}>\n<>\n<z>\n<03>\n<01>\n<x>\n<y>\n<z>\n");
    assert!(output.stderr.is_empty(), "{output:?}");
}
