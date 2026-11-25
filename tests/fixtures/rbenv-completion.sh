# Exercise the same indexed-array and completion operations as rbenv 1.3.2.
fixture_loaded=yes
_fixture_rbenv() {
  fixture_loaded=changed-in-worker
  COMPREPLY=()
  local word="${COMP_WORDS[COMP_CWORD]}"
  if [ "$COMP_CWORD" -eq 1 ]; then
    COMPREPLY=( $(compgen -W "$(rbenv commands)" -- "$word") )
  else
    local words=("${COMP_WORDS[@]}")
    unset "words[0]"
    unset "words[$COMP_CWORD]"
    local completions=$(rbenv completions "${words[@]}")
    COMPREPLY=( $(compgen -W "$completions" -- "$word") )
  fi
}
complete -F _fixture_rbenv rbenv
