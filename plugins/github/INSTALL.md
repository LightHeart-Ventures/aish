# github Plugin — Installation

Installation, GitHub webhook setup, authentication, and troubleshooting now live
in one place: **[README.md → Quick start](./README.md#quick-start)**.

Short version:

```sh
cp -r plugins/github ~/.aish/plugins/github
chmod +x ~/.aish/plugins/github/handlers/*.sh
export WEBHOOK_BROKER_URL=wss://<your-broker>/ws WEBHOOK_PLUGIN_ID=github
export WEBHOOK_BROKER_SECRET=<same value as the GitHub webhook secret>
aish
```

Then point a GitHub webhook (JSON, with that secret) at
`https://<your-broker>/webhooks/<tenant>/github` and subscribe to Pushes, Pull
requests, Issues, Workflow runs and Releases.
