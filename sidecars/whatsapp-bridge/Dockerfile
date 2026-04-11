FROM node:20-alpine

WORKDIR /app

RUN apk add --no-cache git

COPY package.json /app/package.json
RUN npm install --omit=dev

COPY src /app/src

ENV PORT=3200
ENV BAILEYS_AUTH_DIR=/data/auth

VOLUME ["/data"]
EXPOSE 3200

CMD ["npm", "start"]
