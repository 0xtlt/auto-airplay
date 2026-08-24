FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates coreutils curl shellcheck \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --create-home tester

WORKDIR /source
COPY install.sh ./install.sh
COPY packaging/auto-airplay@.service ./packaging/auto-airplay@.service
COPY tests/install/run.sh ./tests/install/run.sh

RUN shellcheck install.sh tests/install/run.sh
RUN ./tests/install/run.sh
