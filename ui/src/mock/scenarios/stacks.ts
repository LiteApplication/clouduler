// Stack files shared by several scenarios. They only use the compose subset
// the mock's spec reader understands (see ../spec.ts), which is also what the
// real parser accepts.

export const vaultwarden = (network: string, host = 'vault') => `version: "3.8"

networks:
  ${network}: {}

services:
  app:
    image: vaultwarden/server:1.32.0
    restart: always
    networks:
      - ${network}
    environment:
      DOMAIN: https://${host}.example.com
    volumes:
      - fast/vaultwarden:/data
    x-clouduler:
      proxy:
        - host: ${host}
          network: ${network}
          port: 80
`;

export const nextcloud = (network: string) => `version: "3.8"

networks:
  ${network}: {}

services:
  app:
    image: nextcloud:29-apache
    restart: always
    depends_on: [db, redis]
    networks:
      - ${network}
    secrets:
      - nextcloud-db-password
    volumes:
      - bulk/nextcloud:/var/www/html
    x-clouduler:
      proxy:
        - host: cloud
          network: ${network}
          port: 80
          max_body_size: 10GB
  db:
    image: postgres:16
    restart: always
    networks:
      - ${network}
    secrets:
      - nextcloud-db-password
    volumes:
      - fast/nextcloud-db:/var/lib/postgresql/data
  redis:
    image: redis:7-alpine
    restart: always
    networks:
      - ${network}

secrets:
  nextcloud-db-password: {}
`;

export const immich = (network: string) => `version: "3.8"

networks:
  ${network}: {}

services:
  server:
    image: ghcr.io/immich-app/immich-server:v1.118.0
    restart: always
    depends_on: [db, redis]
    networks:
      - ${network}
    volumes:
      - bulk/immich-library:/usr/src/app/upload
    x-clouduler:
      proxy:
        - host: photos
          network: ${network}
          port: 2283
  machine-learning:
    image: ghcr.io/immich-app/immich-machine-learning:v1.118.0
    restart: always
    networks:
      - ${network}
    deploy:
      replicas: 1
  redis:
    image: redis:7-alpine
    restart: always
    networks:
      - ${network}
  db:
    image: tensorchord/pgvecto-rs:pg16-v0.3.0
    restart: always
    networks:
      - ${network}
    volumes:
      - fast/immich-db:/var/lib/postgresql/data
`;

export const headscale = (network: string) => `version: "3.8"

networks:
  ${network}: {}

services:
  headscale:
    image: headscale/headscale:0.23.0
    restart: always
    networks:
      - ${network}
    volumes:
      - fast/headscale:/var/lib/headscale
    x-clouduler:
      proxy:
        - host: hs
          network: ${network}
          port: 8080
  ui:
    image: ghcr.io/gurucomputing/headscale-ui:2024.10.10
    restart: always
    networks:
      - ${network}
    x-clouduler:
      proxy:
        - host: hs
          network: ${network}
          port: 8080
          path_prefix: /web
`;

export const notes = (network: string) => `version: "3.8"

networks:
  ${network}: {}

services:
  app:
    image: ghcr.io/silverbulletmd/silverbullet:0.10.1
    restart: always
    networks:
      - ${network}
    volumes:
      - fast/notes:/space
    x-clouduler:
      proxy:
        - host: notes
          network: ${network}
          port: 3000
`;
