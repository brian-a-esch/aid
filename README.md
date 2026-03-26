# Some tooling for AI 

aid is agent management tooling to make life easier

## Repo Management System

This system reads from a common unix style config directory (like neovim does)
`~/.config/aid/projects`, where each file in there is a `.toml` describing how
to setup a repository. These toml files willl explain how to clone, build and
run periodic updates for a given project. To do this we start a background
process to provision at least one cloned & built directory per project the user
has. This will also run periodic background pulls & rebuild the project, so the
user always has a ready version of the repo. To start this run

```bash
# starts the background process which creates the repos
aid server
```

For users we will use this tool interactively to "hand out" ready project
directories. The commands we have are

```bash
# allocates a project to the user, they refer to it via the checkout_name
aid add <project_name> <checkout_name>

# lists all the projects, both in use and in background 
aid list

# lists the actively checked out projects
aid list --active

# lists the background projects & their status (last pull time, build, etc)
aid list --backgorund

# remove a project the user is done with, double checks that there are no local changes
aid rm <checkout_name> 

# skips the double check
aid rm --force <checkout_name>

# removes extra background directories so we get down to
aid cleanup

# removes all background directories
aid purge
```

### Configuration

The config file lives at `~/.config/aid/config.toml`. Each `[[projects]]` entry
describes a repository to clone, build, and keep fresh.

```toml
# Global configs (all optional)
nslots = 2  # number of ready copies to keep per project
refresh_interval_secs = 1800  # how often to pull & rebuild (default: 30 min)

[[projects]]
name = "myproject"
repo_url = "https://github.com/org/myproject"
branch = "main"  # optional, defaults to "main"
nslots = 3  # optional per-project override
has_submodules = false  # deafult

# Each entry is split on whitespace into program + arguments and run directly
# (no shell). Steps execute in order; the first failure aborts the build.
build_command = [
    "make build",
    "make test",
    "make clang-tidy",
]

[[projects]]
name = "other-project"
repo_url = "https://github.com/org/other"
# No build_command means the clone is used as-is with no build step.
```

### Issues
- Need tests
- refresh_slot git update command is not ideal
- No submodule support
- Need to have error model in place for when repo update or build fails. A
  retry loop for poll makes sense. Build failing once seems like it should be
  an error. Initial clone failing seems like a problem. Regardless, repos in an
  error'ed state need to stop having work done, but count against our project
  quota
- Add handshake with protocol version & reject clients with wrong version
- Add api typedefs, we're going to write client
- Hot reload config

## Future Ideas

Some future work I'd like to do

### Agent Runner

This must be a _library_ we build. By being a library, we can make it
customizable & configurable. We need to be able to customize prompts, tools,
agent "modes" (like giving it different tasks build/plan/debug) tools This has
a fringe benefit so we can reuse it in different frontend environments.

