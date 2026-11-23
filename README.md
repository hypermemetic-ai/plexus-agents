# Plexus Agents

Install the machine host on x86-64 Linux (glibc 2.34 or later) with Cargo and cargo-binstall:

```sh
cargo binstall --git https://github.com/hypermemetic-ai/plexus-agents.git \
  --version 0.1.0 --only-signed --strategies crate-meta-data plexus-agents
```

This downloads the signed executable from [GitHub Releases](https://github.com/hypermemetic-ai/plexus-agents/releases/tag/v0.1.0). Each release records its source revision, image ID, and executable digest in `BUILD.json`. The v0.1.0 binary comes from revision `991f511da8100e0a45d13b4413da890d62e2c65f`, the service image deployed on agents-test.

For pairing, follow the [public agent setup guide](https://menger.sh/guide/connect-an-agent.md).
