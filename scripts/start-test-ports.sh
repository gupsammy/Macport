#!/bin/bash

# Macport Test Environment Setup Script
# Starts various services on different ports for testing macport functionality

set -e

GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

PIDS_FILE="/tmp/macport-test-services.pids"
REPOS_DIR="$HOME/Documents/GitHub/Hackathons"

echo -e "${BLUE}╔════════════════════════════════════════╗${NC}"
echo -e "${BLUE}║  Macport Test Environment Setup       ║${NC}"
echo -e "${BLUE}╔════════════════════════════════════════╗${NC}"
echo ""

# Function to check if port is in use
check_port() {
    lsof -i ":$1" -P | grep LISTEN > /dev/null 2>&1
}

# Function to cleanup on exit
cleanup() {
    echo ""
    echo -e "${YELLOW}Cleaning up...${NC}"
    if [ -f "$PIDS_FILE" ]; then
        while IFS= read -r line; do
            pid=$(echo "$line" | cut -d: -f1)
            if ps -p "$pid" > /dev/null 2>&1; then
                kill "$pid" 2>/dev/null || true
            fi
        done < "$PIDS_FILE"
        rm "$PIDS_FILE"
    fi

    # Stop Docker containers
    docker ps -q --filter "name=macport-test-" | xargs -r docker stop > /dev/null 2>&1 || true
    docker ps -aq --filter "name=macport-test-" | xargs -r docker rm > /dev/null 2>&1 || true
}

# Function to start service and track PID
start_service() {
    local port=$1
    local type=$2
    local desc=$3
    local cmd=$4

    if check_port "$port"; then
        echo -e "${YELLOW}⚠️  Port $port already in use, skipping...${NC}"
        return 1
    fi

    echo -e "${BLUE}Starting $desc on port $port...${NC}"
    eval "$cmd" > /dev/null 2>&1 &
    local pid=$!

    # Wait a bit to ensure it started
    sleep 0.5

    if ps -p "$pid" > /dev/null 2>&1; then
        echo "$pid:$port:$type:$desc" >> "$PIDS_FILE"
        echo -e "${GREEN}✅ $desc running on port $port (PID: $pid)${NC}"
        return 0
    else
        echo -e "${RED}❌ Failed to start $desc on port $port${NC}"
        return 1
    fi
}

# Function to display usage
usage() {
    echo "Usage: $0 [command]"
    echo ""
    echo "Commands:"
    echo "  start    - Start all test services (default)"
    echo "  stop     - Stop all test services"
    echo "  status   - Show running test services"
    echo "  help     - Show this help message"
    echo ""
}

# Function to show status
show_status() {
    echo -e "${BLUE}╔════════════════════════════════════════╗${NC}"
    echo -e "${BLUE}║  Running Test Services                 ║${NC}"
    echo -e "${BLUE}╚════════════════════════════════════════╝${NC}"
    echo ""

    if [ ! -f "$PIDS_FILE" ]; then
        echo -e "${YELLOW}No services tracked by this script${NC}"
        echo ""
        echo "Currently listening ports monitored by macport:"
        lsof -i -P | grep -E ':(3000|3001|3002|3306|4000|5000|5173|5432|6379|8000|8080|9000|27017).*LISTEN' | awk '{print $1 "\t" $2 "\t" $9}' | column -t || echo "None found"
        return
    fi

    echo -e "${GREEN}Services started by this script:${NC}"
    while IFS= read -r line; do
        pid=$(echo "$line" | cut -d: -f1)
        port=$(echo "$line" | cut -d: -f2)
        type=$(echo "$line" | cut -d: -f3)
        desc=$(echo "$line" | cut -d: -f4)

        if ps -p "$pid" > /dev/null 2>&1; then
            echo -e "  ${GREEN}✓${NC} Port $port: $desc (PID: $pid)"
        else
            echo -e "  ${RED}✗${NC} Port $port: $desc (PID: $pid - not running)"
        fi
    done < "$PIDS_FILE"

    echo ""
    echo -e "${GREEN}All monitored ports:${NC}"
    lsof -i -P | grep -E ':(3000|3001|3002|3306|4000|5000|5173|5432|6379|8000|8080|9000|27017).*LISTEN' | awk '{print $1 "\t" $2 "\t" $9}' | column -t || echo "None found"
}

# Function to start all services
start_all() {
    # Clean up any existing tracking file
    rm -f "$PIDS_FILE"

    echo -e "${GREEN}Starting test services...${NC}"
    echo ""

    # Start Node.js servers (if projects exist)
    if [ -d "$REPOS_DIR/ditherbaby" ]; then
        start_service 3000 "node" "Next.js (ditherbaby)" \
            "cd $REPOS_DIR/ditherbaby && npm run dev"
    fi

    if [ -d "$REPOS_DIR/image-editor" ]; then
        start_service 3001 "node" "React/Vite (image-editor)" \
            "cd $REPOS_DIR/image-editor && npm run dev"
    fi

    # Start Python HTTP servers
    start_service 5000 "python" "Python HTTP Server" \
        "python3 -m http.server 5000"

    start_service 5173 "python" "Python HTTP Server (Vite port)" \
        "python3 -m http.server 5173"

    start_service 8080 "python" "Python HTTP Server" \
        "python3 -m http.server 8080"

    start_service 9000 "python" "Python HTTP Server" \
        "python3 -m http.server 9000"

    # Start Vite server if bitbybitweb exists
    if [ -d "$REPOS_DIR/bitbybitweb" ]; then
        start_service 8000 "node" "Vite (bitbybitweb)" \
            "cd $REPOS_DIR/bitbybitweb && npm run dev"
    fi

    # Start Redis
    if command -v redis-server &> /dev/null; then
        if ! check_port 6379; then
            echo -e "${BLUE}Starting Redis on port 6379...${NC}"
            redis-server --port 6379 --daemonize yes
            sleep 0.5
            local redis_pid=$(pgrep -f "redis-server.*6379" | head -1)
            if [ -n "$redis_pid" ]; then
                echo "$redis_pid:6379:redis:Redis Server" >> "$PIDS_FILE"
                echo -e "${GREEN}✅ Redis running on port 6379 (PID: $redis_pid)${NC}"
            fi
        else
            echo -e "${YELLOW}⚠️  Port 6379 already in use, skipping Redis...${NC}"
        fi
    else
        echo -e "${YELLOW}⚠️  redis-server not found, skipping...${NC}"
    fi

    # Start Docker containers
    if command -v docker &> /dev/null; then
        echo ""
        echo -e "${BLUE}Starting Docker containers...${NC}"

        # PostgreSQL
        if ! check_port 5432; then
            echo -e "${BLUE}Starting PostgreSQL container...${NC}"
            docker run -d --name macport-test-postgres \
                -p 5432:5432 \
                -e POSTGRES_PASSWORD=test \
                postgres:alpine > /dev/null 2>&1
            echo -e "${GREEN}✅ PostgreSQL running on port 5432${NC}"
        else
            echo -e "${YELLOW}⚠️  Port 5432 already in use, skipping PostgreSQL...${NC}"
        fi

        # MongoDB
        if ! check_port 27017; then
            echo -e "${BLUE}Starting MongoDB container...${NC}"
            docker run -d --name macport-test-mongo \
                -p 27017:27017 \
                mongo:latest > /dev/null 2>&1
            echo -e "${GREEN}✅ MongoDB running on port 27017${NC}"
        else
            echo -e "${YELLOW}⚠️  Port 27017 already in use, skipping MongoDB...${NC}"
        fi
    else
        echo -e "${YELLOW}⚠️  Docker not found, skipping containers...${NC}"
    fi

    echo ""
    echo -e "${GREEN}╔════════════════════════════════════════╗${NC}"
    echo -e "${GREEN}║  Test environment ready!               ║${NC}"
    echo -e "${GREEN}╚════════════════════════════════════════╝${NC}"
    echo ""
    echo "Run './start-test-ports.sh status' to see all services"
    echo "Run './start-test-ports.sh stop' to stop all services"
    echo ""
}

# Function to stop all services
stop_all() {
    echo -e "${BLUE}Stopping test services...${NC}"
    echo ""

    cleanup

    echo -e "${GREEN}✅ All test services stopped${NC}"
}

# Set trap for cleanup
trap cleanup EXIT INT TERM

# Main command processing
case "${1:-start}" in
    start)
        start_all
        # Don't exit so trap doesn't cleanup
        trap - EXIT INT TERM
        ;;
    stop)
        stop_all
        trap - EXIT INT TERM
        ;;
    status)
        show_status
        trap - EXIT INT TERM
        ;;
    help|--help|-h)
        usage
        trap - EXIT INT TERM
        ;;
    *)
        echo -e "${RED}Unknown command: $1${NC}"
        echo ""
        usage
        exit 1
        ;;
esac
