# boxq's Base Image: a generic core with Claude Code preinstalled, and no boxq config, so any
# image with `claude` on the PATH behaves the same (jonaswre/boxlite-quick#29). Language
# toolchains are left to the Setup Hooks.

FROM node:22-bookworm

# The one place Claude Code's version is set. Raising it needs the image workflow's real-Box
# test to pass, which proves boxq's seeded config still gives a prompt-free first run.
ARG CLAUDE_CODE_VERSION=2.1.312

LABEL org.opencontainers.image.source=https://github.com/jonaswre/boxlite-quick
LABEL org.opencontainers.image.description="boxq's Base Image: a generic core with Claude Code"

# gh comes from GitHub's own apt repository, as Debian's lags far behind.
RUN curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg \
        -o /usr/share/keyrings/githubcli-archive-keyring.gpg \
    && echo "deb [arch=amd64 signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main" \
        > /etc/apt/sources.list.d/github-cli.list \
    && apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates fd-find gh git jq less procps python3 ripgrep unzip vim-tiny \
    && ln -s /usr/bin/fdfind /usr/local/bin/fd \
    && rm -rf /var/lib/apt/lists/*

COPY --from=ghcr.io/astral-sh/uv:0.13.6 /uv /uvx /usr/local/bin/

RUN npm install --global --no-fund --no-audit "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
    && npm cache clean --force
