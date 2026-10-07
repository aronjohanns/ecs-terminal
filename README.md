# ecs-terminal

Browse ECS clusters › services › containers › tasks and run the matching AWS CLI commands.

```
cargo run -- [--profile NAME] [--region REGION] [--config PATH]
```

## Config

Linux/macOS: `~/.config/ecs-terminal/config.toml` (or `$XDG_CONFIG_HOME`). Windows: `%APPDATA%\ecs-terminal\config.toml`.

```
mkdir -p ~/.config/ecs-terminal && ecs-terminal --dump-config > ~/.config/ecs-terminal/config.toml
```

Actions per level: `[[ecs]]` `[[cluster]]` `[[service]]` `[[container]]` `[[task]]`. A level you define replaces its defaults.

Placeholders: `{aws}` `{profile}` `{region}` `{cluster}` `{cluster_arn}` `{service}` `{service_arn}` `{task_definition}` `{container}` `{task}` `{task_arn}`. `<TEXT>` prompts before running. `when = "exec_disabled"` hides an action unless ECS Exec is off.

Example, a shell with completion (bash + TERM, falls back to sh):

```toml
[[task]]
title = "Open shell in container"
command = "aws ecs execute-command --cluster {cluster} --task {task} --container {container} --interactive --command \"/bin/sh -c 'export TERM=xterm-256color; exec /bin/bash -l || exec /bin/sh'\" {aws}"
```
