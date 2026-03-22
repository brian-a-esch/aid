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


## Future Ideas

Some future work I'd like to do

### Agent Runner

This must be a _library_ we build. By being a library, we can make it
customizable & configurable. We need to be able to customize prompts, tools,
agent "modes" (like giving it different tasks build/plan/debug) tools This has
a fringe benefit so we can reuse it in different frontend environments.

