FROM debian:bookworm-20261020-slim@sha256:7ff6ba31527b9e336162fa061746c366c4b2032f4d2cff2078bea43f78efe3f8

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
