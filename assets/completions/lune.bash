# Bash completion for lune

_lune_themes() {
    local themes="system dark light catppuccin-mocha catppuccin-latte tokyo-night tokyo-night-day gruvbox-dark gruvbox-light nord dracula rose-pine rose-pine-dawn kanagawa one-dark"
    local dir="${XDG_CONFIG_HOME:-$HOME/.config}/lune/themes"
    if [[ -d "$dir" ]]; then
        local file name
        for file in "$dir"/*.conf; do
            [[ -e "$file" ]] || continue
            name="${file##*/}"
            themes+=" ${name%.conf}"
        done
    fi
    printf '%s\n' "$themes"
}

_lune() {
    local cur prev words cword
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"

    # Stop completing Lune options after `--` or `-e` / `--command`.
    local i subcmd=""
    for ((i = 1; i < COMP_CWORD; i++)); do
        case "${COMP_WORDS[i]}" in
            --|-e|--command)
                COMPREPLY=($(compgen -c -- "$cur"))
                return 0
                ;;
        esac
    done

    if [[ "${COMP_WORDS[1]:-}" == "msg" ]]; then
        # Find the msg subcommand (skipping `--socket <path>`).
        for ((i = 2; i < COMP_CWORD; i++)); do
            case "${COMP_WORDS[i]}" in
                --socket)
                    ((i++))
                    ;;
                --socket=*)
                    ;;
                -*)
                    ;;
                *)
                    subcmd="${COMP_WORDS[i]}"
                    break
                    ;;
            esac
        done

        case "$prev" in
            --socket)
                COMPREPLY=($(compgen -f -- "$cur"))
                return 0
                ;;
            -d|--working-directory)
                COMPREPLY=($(compgen -d -- "$cur"))
                return 0
                ;;
            -T|--title)
                return 0
                ;;
        esac

        if [[ -z "$subcmd" ]]; then
            local opts="--socket --help -h new-tab split set-theme reload-config save-config list"
            COMPREPLY=($(compgen -W "$opts" -- "$cur"))
            return 0
        fi

        case "$subcmd" in
            new-tab)
                if [[ "$cur" == -* ]]; then
                    COMPREPLY=($(compgen -W "-d --working-directory -T --title -e --command --socket" -- "$cur"))
                fi
                ;;
            split)
                if [[ "$cur" == -* ]]; then
                    COMPREPLY=($(compgen -W "--right --down -d --working-directory --socket" -- "$cur"))
                else
                    COMPREPLY=($(compgen -W "right down vertical horizontal" -- "$cur"))
                fi
                ;;
            set-theme)
                if [[ "$cur" == -* ]]; then
                    COMPREPLY=($(compgen -W "--socket" -- "$cur"))
                else
                    COMPREPLY=($(compgen -W "$(_lune_themes)" -- "$cur"))
                fi
                ;;
            reload-config|save-config|list)
                if [[ "$cur" == -* ]]; then
                    COMPREPLY=($(compgen -W "--socket" -- "$cur"))
                fi
                ;;
        esac
        return 0
    fi

    case "$prev" in
        -d|--working-directory)
            COMPREPLY=($(compgen -d -- "$cur"))
            return 0
            ;;
        -c|--config)
            COMPREPLY=($(compgen -f -- "$cur"))
            return 0
            ;;
        -T|--title|--class|--app-id)
            return 0
            ;;
    esac

    if [[ "$COMP_CWORD" -eq 1 && "$cur" != -* ]]; then
        COMPREPLY=($(compgen -W "msg" -- "$cur"))
        return 0
    fi

    local opts="-d --working-directory -T --title --class --app-id -c --config -e --command -h --help -V --version"
    COMPREPLY=($(compgen -W "$opts" -- "$cur"))
}

complete -F _lune lune
