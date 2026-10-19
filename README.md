# Status agent

This is an agent that runs on my VMs and checks that my projects are up.

The agent periodically requests a list of URLs, which are grouped into projects.
If a URL fails a number of consecutive checks it is considered down.
A project is down if any of its checks are down.

When a project goes down the agent sends an email.
Projects that are down at the same time are grouped into one outage
    with a single email thread,
    and the agent replies to the thread as projects go down and recover.
Emails only describe projects and link to the status page for details;
    checks going up or down within a project that is already down don't send emails.

The agent serves a status page listing the projects.
Each project's page shows its checks and incidents,
    and each check's page shows recent results and daily uptime over the last 90 days.

The projects to check are specified using a config file.
An example of this config file is `config.yml` and the full spec with documentation
    is at `src/config.rs`.
Projects can also live in their own files, listed under `include` in the main config;
    each included file contains one project.
Projects with the same name are merged,
    so a project can be split across files and across agents.

To run the agent in the repository root, simply run
    `cargo run -- --mode=agent --config=$PATH_TO_CONFIG_FILE`.

To persist the state of the agent across runs,
    including the history of checks and incidents,
    pass the `--db=some/file.txt` flag.
State will be saved in that file.

## Metrics

The agent serves Prometheus metrics at `/metrics`.
The main ones are `status_project_up` and `status_check_up` (1 if up, 0 if down),
    `status_checks_total` (by project, check and result),
    and `status_check_duration_seconds`.
`status_poll_loop_last_run_timestamp_seconds` can be used to alert if the agent itself stops checking.
All of the metrics are documented in `src/metrics.rs`.

## UI mode

Running with `--mode=ui --agent=status1.example.com --agent=status2.example.com ...`
    serves a page that aggregates the status of several agents.
The page fetches `/status.json` from each agent in the browser,
    so the agents must be reachable from wherever the page is viewed.
The agent sets `Access-Control-Allow-Origin: *` on `/status.json` to allow this.
Projects with the same name on several agents are shown as one project.
Set `ui_url` in each agent's config to the aggregate page so that links in emails go there.

## Deploying the agent

A status agent can't report an outage of the machine it runs on.
So I run an agent on each of my VMs,
    and each agent checks the projects running on _other_ VMs.
Each agent also checks the agents on the other VMs,
    so I find out if an agent itself stops working.

## License

MIT
