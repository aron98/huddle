# Huddle recording muxer (issue #15 ffmpeg sidecar).
#
# Node runs deploy/recording/mux.mjs, which replays the server's
# length-framed .rtp captures into ffmpeg's SDP demuxer over loopback UDP
# and renders the tiles + mixed audio to a single .mp4. See
# docker-compose.yml (service `mux`) for how it is driven.

FROM node:22-bookworm-slim

RUN apt-get update -qq \
    && apt-get install -qq -y --no-install-recommends ffmpeg \
    && rm -rf /var/lib/apt/lists/*

COPY mux.mjs /usr/local/bin/huddle-mux

ENTRYPOINT ["node", "/usr/local/bin/huddle-mux"]
