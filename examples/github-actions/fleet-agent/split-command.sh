# shellcheck shell=bash
# Splits a Maestro command line into words for action.yml. Quotes and backslashes work as they do
# in a shell (`maestro test "my flows/"` gives two arguments after `test`); nothing is expanded, so
# `$HOME` and `*` stay literal, and nothing is evaluated. Sourced, not run.
#
# split_command STRING fills the array WORDS and returns 1 for an unterminated quote.
split_command() {
  local input=$1 word='' quote='' in_word=0 i c next
  WORDS=()
  for ((i = 0; i < ${#input}; i++)); do
    c=${input:i:1}
    if [[ -n $quote ]]; then
      if [[ $c == "$quote" ]]; then
        quote=''
      elif [[ $quote == '"' && $c == \\ ]]; then
        # Inside double quotes a backslash escapes only $, `, " and itself.
        next=${input:i+1:1}
        case $next in
          '$' | '`' | '"' | \\) word+=$next; ((i++)) ;;
          *) word+=$c ;;
        esac
      else
        word+=$c
      fi
    else
      case $c in
        ' ' | $'\t' | $'\n')
          if ((in_word)); then
            WORDS+=("$word")
            word=''
            in_word=0
          fi
          ;;
        "'" | '"')
          quote=$c
          in_word=1
          ;;
        \\)
          # Outside quotes a backslash makes the next character literal, even a space.
          next=${input:i+1:1}
          word+=$next
          ((i++))
          in_word=1
          ;;
        *)
          word+=$c
          in_word=1
          ;;
      esac
    fi
  done
  [[ -z $quote ]] || return 1
  if ((in_word)); then
    WORDS+=("$word")
  fi
}
