# Status agent

This is an agent that runs on my VMs and checks that my projects are up.

The agent periodically requests a list of URLs.
If a URL fails a number of consecutive checks it is considered down,
    and the agent sends an email.
When it starts responding again the agent sends another email.
The agent serves a status page showing the current state of each URL,
    recent incidents, and daily uptime over the last 90 days.

The URLs to check are specified using a config file.
An example of this config file is `config.yml` and the full spec with documentation
    is at `src/config.rs`.
Checks can also live in their own files, listed under `include` in the main config;
    each included file contains a list of checks.

To run the agent in the repository root, simply run
    `cargo run -- --mode=agent --config=$PATH_TO_CONFIG_FILE`.

To persist the state of the agent across runs,
    including the history of checks and incidents,
    pass the `--db=some/file.txt` flag.
State will be saved in that file.

## Status page views

Each check has one of two views on the status page,
    set using the `view` field in its config:

- `status` (the default): only the current state.
- `history`: the current state and a bar of daily uptime over the last 90 days.

The View toggle at the top of the page switches all checks to either view.

## Metrics

The agent serves Prometheus metrics at `/metrics`.
The main ones are `status_check_up` (1 if up, 0 if down),
    `status_checks_total` (by check and result),
    and `status_check_duration_seconds`.
`status_poll_loop_last_run_timestamp_seconds` can be used to alert if the agent itself stops checking.
All of the metrics are documented in `src/metrics.rs`.

## UI mode

Running with `--mode=ui --agent=status1.example.com --agent=status2.example.com ...`
    serves a page that aggregates the status of several agents.
The page fetches `/status.json` from each agent in the browser,
    so the agents must be reachable from wherever the page is viewed.
The agent sets `Access-Control-Allow-Origin: *` on `/status.json` to allow this.

## Deploying the agent

A status agent can't report an outage of the machine it runs on.
So I run an agent on each of my VMs,
    and each agent checks the projects running on _other_ VMs.
Each agent also checks the agents on the other VMs,
    so I find out if an agent itself stops working.

## License

MIT
