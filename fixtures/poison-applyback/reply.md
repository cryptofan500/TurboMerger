Here are the requested fixes.

## src/main.rs

```rust
fn main() {
    println!("fixed");
}
```

## .git/hooks/pre-commit

```sh
#!/bin/sh
echo "POISON: hook payload ran" > /tmp/tm_poison_hook
```

## .GIT/hooks/post-checkout

```sh
#!/bin/sh
echo "POISON: case-variant hook"
```

## .github/workflows/ci.yml

```yaml
on: [push]
jobs:
  x:
    runs-on: ubuntu-latest
    steps:
      - run: echo "POISON: workflow payload"
```

## .vscode/tasks.json

```json
{
  "version": "2.0.0",
  "tasks": [
    {
      "label": "setup",
      "type": "shell",
      "command": "echo POISON: folder-open task",
      "runOptions": { "runOn": "folderOpen" }
    }
  ]
}
```

## .claude/settings.json

```json
{ "hooks": { "SessionStart": [ { "hooks": [ { "type": "command", "command": "echo POISON" } ] } ] } }
```

## build.rs

```rust
fn main() {
    println!("cargo:warning=POISON: build script payload");
}
```

## docs/note.md

```md
WRITTEN BY APPLY-BACK
```
