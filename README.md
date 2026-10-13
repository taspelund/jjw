# jjw

`jjw` is a local jj workspace manager with GitHub pull-request integration.

## Install

```sh
cargo install --path .
```

The binary depends on `jj`, `git`, `gh`, and `fzf`. Generic workspaces do not
require a GitHub remote and are created under
`$HOME/git/workspaces/<parent>/<repository>-<identity>`, where the identity
prevents collisions between same-named repositories. GitHub-backed commands read the
current repository's `origin` remote and create PR workspaces under
`$HOME/git/workspaces/<owner>/<repository>-<identity>`. Pull requests from forks are
fetched via GitHub's pull refs and compared from the immutable merge-base commit
to the immutable PR head commit. Checking out an existing PR workspace updates
its working-copy commit onto the PR's latest head while preserving local
changes. Both colocated and non-colocated jj/Git repositories are supported.

`create` and `pr checkout` accept repeatable `--share <PATH>` options. Each
path must be relative, exist in the default workspace, and be ignored by Git;
`jjw` symlinks it into the new workspace. Sharing is opt-in and destination
conflicts abort workspace creation without replacing existing data.

## Shell integration

A subprocess cannot change its parent shell's directory. Add this small wrapper
to `.zshrc` so commands that create a workspace can `cd` into the path printed
by the binary:

```zsh
function jjw() {
  if [[ "$1" == "c" || "$1" == "create" || "$1" == "rm" ||
        ("$1" == "pr" &&
          ("$2" == "checkout" || "$2" == "cleanup")) ]]; then
    local workspace_path
    workspace_path="$(command jjw "$@")" || return $?
    if [[ -d "$workspace_path" ]]; then
      cd "$workspace_path"
    elif [[ -n "$workspace_path" ]]; then
      print -r -- "$workspace_path"
    fi
  else
    command jjw "$@"
  fi
}
```

Run `jjw --help` for the complete command list. `ls` includes each workspace's
path. `create` and `pr checkout` print only the selected path to stdout. `rm`
and `pr cleanup` print the default root only when they remove the current
workspace. Removal refuses to delete ignored or otherwise untracked files
unless `--force` is passed. Workspace-root `.git` metadata and known jj
administrative paths (`.jj/repo`, `.jj/working_copy`, `.jj/.gitignore`) do not
require `--force`; other data under `.jj` and nested repositories still do.
A subcommand is required; bare `jjw` prints usage.
Diagnostics go to stderr.

`pr cleanup` confirms before deletion; pass `--yes` for non-interactive use.
Removal protects data present during its final quarantine scan; callers must
stop concurrent writers before deleting a workspace.

## jj alias

To invoke the binary as `jj w`, add a jj command alias:

```sh
jj config set --user aliases.w '["util", "exec", "--", "jjw"]'
```
