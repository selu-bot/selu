FROM node:24-alpine

WORKDIR /app

RUN apk add --no-cache git

COPY package.json package-lock.json /app/
RUN npm ci --omit=dev

COPY src /app/src

ENV PORT=3200
ENV BAILEYS_AUTH_DIR=/data/auth

VOLUME ["/data"]
EXPOSE 3200

CMD ["npm", "start"]
