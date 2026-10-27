# bsnfs in a container.
#
#   docker build -t bsnfs .
#   docker run -d --name bsnfs \
#     -p 2049:2049/tcp -p 2049:2049/udp -p 111:111/tcp -p 111:111/udp -p 8080:8080 \
#     -e BSNFS_ADMIN_PASSWORD=choose-one \
#     -v /path/on/host:/share -v bsnfs-config:/config bsnfs
#
# The admin page listens on all interfaces here, so a password is required:
# the container won't start without BSNFS_ADMIN_PASSWORD.
#
# Add more folders by mounting them into the container and sharing them from
# the admin UI (http://HOST:8080/); UI changes persist in /config/exports.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim
COPY --from=build /src/target/release/bsnfs /usr/local/bin/bsnfs
RUN mkdir -p /share /config
VOLUME ["/config"]
# 111: portmapper, 2049: NFS + MOUNT + lock manager, 8080: admin UI
EXPOSE 111/tcp 111/udp 2049/tcp 2049/udp 8080/tcp
ENV BSNFS_ADMIN_PASSWORD=""
ENTRYPOINT ["bsnfs", "--admin", "0.0.0.0:8080", "--exports-file", "/config/exports"]
CMD ["share=/share"]
