# Isolated Linux environment for the public async-profiler test harness.
FROM eclipse-temurin:21-jdk
RUN apt-get update \
 && apt-get install -y --no-install-recommends build-essential python3 rustc cargo \
 && rm -rf /var/lib/apt/lists/*
ENV JAVA_HOME=/opt/java/openjdk
