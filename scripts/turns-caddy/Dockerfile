# Caddy with the caddy-l4 plugin, as the self-hosting guide builds it.
FROM caddy:2-builder AS build
RUN xcaddy build --with github.com/mholt/caddy-l4
FROM caddy:2
COPY --from=build /usr/bin/caddy /usr/bin/caddy
