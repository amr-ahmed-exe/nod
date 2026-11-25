FROM node:22-alpine AS ui-build
WORKDIR /app
RUN corepack enable
COPY package.json pnpm-lock.yaml pnpm-workspace.yaml ./
COPY patches ./patches
RUN pnpm install --frozen-lockfile --ignore-scripts
COPY tsconfig.json vite.config.ts index.html ./
COPY src ./src
COPY desktop/ui ./desktop/ui
COPY desktop/scripts/build-ui.mjs ./desktop/scripts/build-ui.mjs
COPY public ./public
RUN ./node_modules/.bin/tsc --noEmit && ./node_modules/.bin/vite build && node desktop/scripts/build-ui.mjs

FROM rust:1-alpine AS rs-build
RUN apk add --no-cache musl-dev
WORKDIR /app
COPY src/apps ./src/apps
COPY nod-rs ./nod-rs
COPY --from=ui-build /app/dist/web ./nod-rs/web
COPY --from=ui-build /app/dist/desktop-ui/index.html ./nod-rs/web/setup.html
COPY --from=ui-build /app/dist/desktop-ui/setup.css ./nod-rs/web/setup.css
COPY --from=ui-build /app/dist/desktop-ui/setup.js ./nod-rs/web/setup.js
COPY --from=ui-build /app/dist/desktop-ui/styles.css ./nod-rs/web/styles.css
COPY --from=ui-build /app/dist/desktop-ui/Geist-Variable.woff2 ./nod-rs/web/Geist-Variable.woff2
RUN cargo build --manifest-path nod-rs/Cargo.toml --release --no-default-features

FROM alpine:3.21
RUN apk add --no-cache ca-certificates libstdc++ sqlite-libs tzdata curl
WORKDIR /app
COPY --from=rs-build /app/nod-rs/target/release/nod /usr/local/bin/nod
RUN addgroup -S nod && adduser -S -G nod nod && mkdir -p /var/lib/nod && chown -R nod:nod /var/lib/nod
USER nod
ENV NOD_DATA_DIR=/var/lib/nod
ENV NOD_HEADLESS=1
ENV NOD_HOST=0.0.0.0
ENV NOD_PORT=4310
EXPOSE 4310
HEALTHCHECK --interval=15s --timeout=5s --retries=10 CMD curl -fsS http://127.0.0.1:${NOD_PORT}/api/health || exit 1
CMD ["/usr/local/bin/nod", "--headless"]
