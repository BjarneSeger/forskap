# forskap-cli
A CLI for GitLab, served from the cache of `forskapd`.

Besides issues, merge requests and search, forskap integrates time tracking into your
workflow by regularly asking you what you worked on, in the terminal.

# Setup

## Installation
Prebuilt binaries are available in the releases and packages are built for debian,
rpm and arch.
After installing the package, make sure to enable the systemd socket:

```sh
systemctl enable --now --user forskapd.socket
```


```sh
forskap time hook <SHELL>
```

to get the snippet to add to your respective shellrc. After that, you will be asked
after 30 minutes when the next cli prompt should appear what you are working on, 
with a list of assigned issues.

### Completions
Out of the box, completions are installed for fish, zsh and bash. Completions are
also provided for carapace, but they need to be manually linked from
`/usr/share/carapace/specs/forskap.yaml` to `~/.config/carapace/specs/forskap.yaml`, as
carapace does not currently support globally installed specs.

## Config
The config lives at `$XDG_CONFIG_HOME/` or `$HOME/.config/` under
`forskap/config.toml`. You can run `forskap config path` to see what it 
resolves to on your system. To get a sample config, run

```sh
forskap config template
# Save the default config
# forskap config template > $(forskap config path)
```
