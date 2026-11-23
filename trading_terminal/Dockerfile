# Multi-stage build: Vite/React SPA -> nginx static server + reverse proxy.
# The frontend never depends on the Rust crates; this image only ships
# dist/ plus the nginx proxy config.

FROM docker.io/library/node:22-alpine AS build
WORKDIR /app
COPY package.json package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY . .
RUN npm run build

FROM docker.io/nginxinc/nginx-unprivileged:1.27-alpine
COPY --from=build /app/dist /usr/share/nginx/html
COPY nginx/default.conf.template /etc/nginx/templates/default.conf.template
ENV MD_UPSTREAM=http://global-marketdata.match-eu-1.svc.cluster.local:24804 \
    DESK_UPSTREAM=http://orders.match-eu-1.svc.cluster.local:24300 \
    DNS_UPSTREAM=127.0.0.11
EXPOSE 8080
