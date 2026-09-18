# A genuinely fresh machine: nothing of ours installed, nobody logged in.
# No compiler/git/ssh/credential tooling on purpose — the whole point of this
# image is that install.sh must work on a box that has none of that, so
# adding any of it back would let a passing test hide a real dependency.
#
# Build from the repo root (COPY paths below are relative to repo root):
#   docker build -t remuda-cold-room:dry-fit -f scripts/cold-room/Dockerfile .
FROM debian:bookworm-slim

RUN apt-get update \
	&& apt-get install -y --no-install-recommends curl ca-certificates tar \
	&& rm -rf /var/lib/apt/lists/*

COPY docs/install.sh /opt/remuda-repo/docs/install.sh
COPY scripts/check-clean-machine.sh /opt/remuda-repo/scripts/check-clean-machine.sh
COPY scripts/check-cold-install.sh /opt/remuda-repo/scripts/check-cold-install.sh

WORKDIR /opt/remuda-repo

CMD ["sh"]
