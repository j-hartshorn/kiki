# Worktrees

Use one branch and one worktree for each change. The root checkout stays on `main`.

```sh
git worktree add -b work/<descriptive-slug> worktrees/<descriptive-slug> main
cd worktrees/<descriptive-slug>
```

Commit and test the change in its worktree. After the user accepts it, return to the root checkout and merge with a merge commit:

```sh
git switch main
git merge --no-ff work/<descriptive-slug>
git worktree remove worktrees/<descriptive-slug>
git branch -d work/<descriptive-slug>
```

Use the same descriptive slug for the branch and directory. Remove a discarded worktree with `git worktree remove worktrees/<descriptive-slug>` and then `git branch -D work/<descriptive-slug>`.
