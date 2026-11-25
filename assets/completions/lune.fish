# Fish completion for lune

function __lune_themes
    printf '%s\n' \
        "system	Follow desktop preference" \
        "dark	Built-in dark theme" \
        "light	Built-in light theme" \
        "catppuccin-mocha	Built-in dark theme" \
        "catppuccin-latte	Built-in light theme" \
        "tokyo-night	Built-in dark theme" \
        "tokyo-night-day	Built-in light theme" \
        "gruvbox-dark	Built-in dark theme" \
        "gruvbox-light	Built-in light theme" \
        "nord	Built-in dark theme" \
        "dracula	Built-in dark theme" \
        "rose-pine	Built-in dark theme" \
        "rose-pine-dawn	Built-in light theme" \
        "kanagawa	Built-in dark theme" \
        "one-dark	Built-in dark theme"

    set -l config_home (set -q XDG_CONFIG_HOME; and echo $XDG_CONFIG_HOME; or echo "$HOME/.config")
    set -l dir "$config_home/lune/themes"
    if test -d "$dir"
        for file in "$dir"/*.conf
            test -e "$file"; or continue
            set -l name (string replace -r '\.conf$' '' -- (basename -- "$file"))
            printf '%s\tCustom theme\n' "$name"
        end
    end
end

function __lune_is_msg
    set -l tokens (commandline -opc)
    test (count $tokens) -ge 2; and test "$tokens[2]" = "msg"
end

function __lune_msg_subcmd
    set -l tokens (commandline -opc)
    set -l i 3
    while test $i -le (count $tokens)
        switch $tokens[$i]
            case --socket
                set i (math $i + 2)
                continue
            case '--socket=*' '-*'
                set i (math $i + 1)
                continue
            case '*'
                echo $tokens[$i]
                return 0
        end
    end
    return 1
end

# Top-level options and subcommand
complete -c lune -n "not __lune_is_msg" -s d -l working-directory -r -a "(__fish_complete_directories)" -d "Start the initial session in DIR"
complete -c lune -n "not __lune_is_msg" -s T -l title -r -d "Set the initial window title"
complete -c lune -n "not __lune_is_msg" -l class -r -d "Set the Wayland application ID"
complete -c lune -n "not __lune_is_msg" -l app-id -r -d "Set the Wayland application ID"
complete -c lune -n "not __lune_is_msg" -s c -l config -r -F -d "Load configuration from PATH"
complete -c lune -n "not __lune_is_msg" -s e -l command -r -d "Run CMD instead of the default shell"
complete -c lune -n "not __lune_is_msg" -s h -l help -d "Print help message and exit"
complete -c lune -n "not __lune_is_msg" -s V -l version -d "Print version information and exit"
complete -c lune -n "not __lune_is_msg; and test (count (commandline -opc)) -eq 1" -f -a "msg" -d "Control a running Lune window"

# `lune msg` subcommands
complete -c lune -n "__lune_is_msg" -l socket -r -F -d "Target IPC socket path"
complete -c lune -n "__lune_is_msg; and not __lune_msg_subcmd >/dev/null" -f -a "new-tab" -d "Open a new tab"
complete -c lune -n "__lune_is_msg; and not __lune_msg_subcmd >/dev/null" -f -a "split" -d "Split the focused pane"
complete -c lune -n "__lune_is_msg; and not __lune_msg_subcmd >/dev/null" -f -a "set-theme" -d "Switch the active theme"
complete -c lune -n "__lune_is_msg; and not __lune_msg_subcmd >/dev/null" -f -a "reload-config" -d "Reload configuration"
complete -c lune -n "__lune_is_msg; and not __lune_msg_subcmd >/dev/null" -f -a "save-config" -d "Save current settings to config"
complete -c lune -n "__lune_is_msg; and not __lune_msg_subcmd >/dev/null" -f -a "list" -d "List active IPC sockets"

# `lune msg new-tab`
complete -c lune -n "__lune_is_msg; and test (__lune_msg_subcmd) = new-tab" -s d -l working-directory -r -a "(__fish_complete_directories)" -d "Start the new tab in DIR"
complete -c lune -n "__lune_is_msg; and test (__lune_msg_subcmd) = new-tab" -s T -l title -r -d "Set a custom tab title"
complete -c lune -n "__lune_is_msg; and test (__lune_msg_subcmd) = new-tab" -s e -l command -r -d "Run CMD instead of the default shell"

# `lune msg split`
complete -c lune -n "__lune_is_msg; and test (__lune_msg_subcmd) = split" -l right -d "Split into side-by-side columns"
complete -c lune -n "__lune_is_msg; and test (__lune_msg_subcmd) = split" -l down -d "Split into stacked rows"
complete -c lune -n "__lune_is_msg; and test (__lune_msg_subcmd) = split" -s d -l working-directory -r -a "(__fish_complete_directories)" -d "Start the split pane in DIR"
complete -c lune -n "__lune_is_msg; and test (__lune_msg_subcmd) = split" -f -a "right down vertical horizontal" -d "Split direction"

# `lune msg set-theme`
complete -c lune -n "__lune_is_msg; and test (__lune_msg_subcmd) = set-theme" -f -a "(__lune_themes)" -d "Theme name"
