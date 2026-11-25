# Nushell completion for lune

def "nu-complete lune themes" [] {
    let builtin = [
        { value: "system", description: "Follow desktop light/dark preference" }
        { value: "dark", description: "Built-in default dark theme" }
        { value: "light", description: "Built-in default light theme" }
        { value: "catppuccin-mocha", description: "Built-in dark theme" }
        { value: "catppuccin-latte", description: "Built-in light theme" }
        { value: "tokyo-night", description: "Built-in dark theme" }
        { value: "tokyo-night-day", description: "Built-in light theme" }
        { value: "gruvbox-dark", description: "Built-in dark theme" }
        { value: "gruvbox-light", description: "Built-in light theme" }
        { value: "nord", description: "Built-in dark theme" }
        { value: "dracula", description: "Built-in dark theme" }
        { value: "rose-pine", description: "Built-in dark theme" }
        { value: "rose-pine-dawn", description: "Built-in light theme" }
        { value: "kanagawa", description: "Built-in dark theme" }
        { value: "one-dark", description: "Built-in dark theme" }
    ]

    let config_home = ($env.XDG_CONFIG_HOME? | default ("~/.config" | path expand))
    let themes_dir = ($config_home | path join "lune" "themes")
    let custom = if ($themes_dir | path exists) {
        glob ($themes_dir | path join "*.conf")
        | each {|file|
            {
                value: ($file | path parse | get stem)
                description: "Custom theme"
            }
        }
    } else {
        []
    }

    $builtin | append $custom
}

def "nu-complete lune split-direction" [] {
    [
        { value: "right", description: "Split into side-by-side columns" }
        { value: "down", description: "Split into stacked rows" }
        { value: "vertical", description: "Alias for right" }
        { value: "horizontal", description: "Alias for down" }
    ]
}

# Launch Lune terminal emulator
export extern "lune" [
    --working-directory (-d): path    # Start the initial session in DIR
    --title (-T): string              # Set the initial window title
    --class: string                   # Set the Wayland application ID
    --app-id: string                  # Set the Wayland application ID
    --config (-c): path               # Load configuration from PATH
    --command (-e): string            # Run CMD instead of the default shell
    --help (-h)                       # Print help message and exit
    --version (-V)                    # Print version information and exit
    ...args: string                   # Optional command arguments
]

# Control a running Lune window over local IPC
export extern "lune msg" [
    --socket: path                    # Target IPC socket path
    --help (-h)                       # Print help message and exit
]

# Open a new tab in a running Lune window
export extern "lune msg new-tab" [
    --socket: path                    # Target IPC socket path
    --working-directory (-d): path    # Start the new tab in DIR
    --title (-T): string              # Set a custom tab title
    --command (-e): string            # Run CMD instead of the default shell
    ...args: string                   # Optional command arguments
]

# Split the focused pane in a running Lune window
export extern "lune msg split" [
    --socket: path                                          # Target IPC socket path
    --right                                                 # Split into side-by-side columns
    --down                                                  # Split into stacked rows
    --working-directory (-d): path                          # Start the split pane in DIR
    direction?: string@"nu-complete lune split-direction"   # Split direction (default: right)
]

# Switch the active theme in a running Lune window
export extern "lune msg set-theme" [
    --socket: path                                  # Target IPC socket path
    theme: string@"nu-complete lune themes"         # Built-in or custom theme name
]

# Reload configuration in a running Lune window
export extern "lune msg reload-config" [
    --socket: path                    # Target IPC socket path
]

# Save current settings to the configuration file
export extern "lune msg save-config" [
    --socket: path                    # Target IPC socket path
]

# List active Lune IPC sockets
export extern "lune msg list" [
    --socket: path                    # Target IPC socket path
]
