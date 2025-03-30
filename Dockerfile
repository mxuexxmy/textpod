FROM rust:alpine AS build

RUN apk add musl-dev perl make
RUN cargo install textpod
RUN cargo install monolith

FROM alpine
COPY --from=build /usr/local/cargo/bin/textpod /usr/bin/textpod
COPY --from=build /usr/local/cargo/bin/monolith /usr/bin/monolith

RUN apk add --no-cache tzdata

WORKDIR /app/notes

EXPOSE 3000/tcp

ENTRYPOINT ["textpod"]
CMD ["-p", "3000", "-l", "0.0.0.0"]
