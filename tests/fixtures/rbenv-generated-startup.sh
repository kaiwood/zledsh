# Shape of the output from rbenv 1.3.2's `rbenv init - bash`, with a local source fixture.
export PATH="${HOME}/.rbenv/shims:${PATH}"
export RBENV_SHELL=bash
source ./mock-completions
command rbenv rehash 2>/dev/null
rbenv() {
  local command
  command="${1:-}"
  if [ "$#" -gt 0 ]; then
    shift
  fi

  case "$command" in
    rehash|shell) eval "$(rbenv "sh-$command" "$@")";;
    *) command rbenv "$command" "$@";;
  esac
}
