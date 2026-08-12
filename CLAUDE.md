# Working in this repository

## Commit on `main`. Do not make branches.

Several people and several agents work in these repos at once. Every branch that has been made
here has cost more than it saved: two independent implementations of the same trigger feature,
a contract change that never reached a tag, and a type orphaned when a branch was deleted after
its pull request had already been squash-merged — the commit was pushed afterwards, so deleting
the branch stranded it, and recovering it took three rounds.

None of that is a branching problem in the abstract. It is what happens when work is spread
across branches nobody else can see while the thing they depend on is moving underneath.

So: **pull, commit, push, on `main`.** If somebody else pushed first, pull and merge — the
conflict is smaller now than it will be in a day.

If a change genuinely cannot be committed in a working state, say so and leave it uncommitted
rather than parking it on a branch. Somebody will ask.

## No `Co-Authored-By` trailers

Do not add `Co-Authored-By: Claude` or any variant to commits. The commit is the author's.

Use `jttelaak@hotmail.com` as the git identity here.

## Never `--force`

Not on `main`, not on a tag anybody could have pulled, not "just this once". If a push is
rejected, pull and merge. A force-push in a repo with concurrent writers destroys work that has
already been fetched by somebody else, and it is not recoverable from their side.

## The SDK: track `main`, do not tag

`driver-sdk` is public and is pinned here to its **branch**:

```toml
driver-sdk = { git = "https://github.com/junohouse/driver-sdk", branch = "main", features = ["pack"] }
```

**Do not cut tags.** This is alpha; there is no release, and nothing outside these repos depends
on a version number. Tagging bought nothing here and cost a great deal: a contract change that
missed a tag, a consumer that would not compile because of it, and four versions cut in one
afternoon chasing types that existed on `main` the whole time.

Changing a contract is therefore two steps, not four: change it in `driver-sdk`, push to `main`.
Consumers pick it up on their next `cargo update -p driver-sdk`.

The cost of a branch pin is that a build is not reproducible from the lockfile alone — the
lockfile still records the exact commit, so a checked-in `Cargo.lock` pins it until somebody
updates deliberately. That is the right trade while the contracts are still moving. Revisit it
when there is something to release.

## Before you say it works

Run it. `cargo test` is the floor, not the ceiling:

- A test that compiles proves the types line up, not that pressing the button moves anything.
- Contract validation proves a manifest is honest about what a device *claims*. Nothing checks
  it is honest about what it *does* unless you write that test.
- Anything timezone-, clock- or network-shaped needs checking under conditions other than this
  machine's. `TZ=UTC cargo test` has caught real bugs here.

If you could not verify something, write that down — in the commit message, and as a comment
where the gap is. A gap nobody recorded reads as protection that is not there.

## Comments explain why

Not what the code does. What goes wrong otherwise, and what the alternative cost. Match the
surrounding style; it is consistent on purpose.
