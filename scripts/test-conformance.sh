#!/bin/bash

# ASCOM Alpaca Conformance Testing Script
# Tests the filemonitor service using ConformU

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# ConformU is resolved at install time, not pinned by default. `conformu.yml` installs
# `latest` on every run (ivonnyssen/conformu-install@v3), so a pinned local copy
# silently falls behind what CI validates against -- and a docs/validation/
# record made on a stale version is evidence for a validator the project has
# moved past (docs/skills/hardware-validation.md).
#
# Set CONFORMU_VERSION=v4.4.0 to pin deliberately, e.g. to reproduce an old run.
CONFORMU_VERSION="${CONFORMU_VERSION:-latest}"
# Linux x64 only. On macOS/Windows, install from the ConformU releases page
# instead: https://github.com/ASCOMInitiative/ConformU/releases
# Upstream ships the Linux builds xz-compressed; there is no .tar.gz asset
# to fall back to, so a wrong extension here fails as a download 404.
CONFORMU_ASSET="conformu.linux-x64.tar.xz"

show_help() {
    echo "Usage: $0 [OPTIONS]"
    echo ""
    echo "Run ASCOM Alpaca conformance tests on filemonitor service"
    echo ""
    echo "Options:"
    echo "  --install-conformu  Install ConformU, latest release by default (Linux x64)"
    echo "  --port PORT         Use specific port (default: 11111)"
    echo "  --config FILE       Use specific config file"
    echo "  --test-dir DIR      Use specific test directory"
    echo "  --keep-reports      Don't delete test reports after completion"
    echo "  --verbose           Verbose output"
    echo "  -h, --help          Show this help"
    echo ""
    echo "Examples:"
    echo "  $0                          # Run conformance tests"
    echo "  $0 --install-conformu       # Install the latest ConformU"
    echo "  CONFORMU_VERSION=v4.4.0 $0 --install-conformu   # pin deliberately"
    echo "  $0 --port 12345 --verbose   # Use custom port with verbose output"
}

# Resolve CONFORMU_VERSION to a concrete release tag. "latest" asks GitHub;
# anything else is taken literally so an old run can be reproduced.
resolve_conformu_version() {
    if [[ "$CONFORMU_VERSION" != "latest" ]]; then
        echo "$CONFORMU_VERSION"
        return
    fi

    local api="repos/ASCOMInitiative/ConformU/releases/latest"
    local tag=""

    # Prefer gh: it carries auth, so it is not subject to the unauthenticated
    # rate limit, and it is what docs/validation/README.md tells you to run.
    if command -v gh >/dev/null 2>&1; then
        tag=$(gh api "$api" --jq '.tag_name // empty' 2>/dev/null || true)
    fi

    # Both tools, matching what the failure message below asks for. curl -f only
    # fails on HTTP >= 400, so a proxy answering 200 with an HTML interstitial
    # reaches jq -- hence jq's stderr is silenced too, leaving only our guidance.
    if [[ -z "$tag" ]] && command -v curl >/dev/null 2>&1 \
        && command -v jq >/dev/null 2>&1; then
        tag=$(curl -fsSL "https://api.github.com/${api}" 2>/dev/null \
            | jq -r '.tag_name // empty' 2>/dev/null || true)
    fi

    if [[ -z "$tag" ]]; then
        echo "ERROR: could not resolve the latest ConformU release." >&2
        echo "Needs gh, or curl + jq, with access to api.github.com." >&2
        echo "Or pin a tag from the releases page:" >&2
        echo "  https://github.com/ASCOMInitiative/ConformU/releases" >&2
        echo "  CONFORMU_VERSION=<tag> $0 --install-conformu" >&2
        return 1
    fi

    echo "$tag"
}

# Download $1 to $2. curl is tried first because it is already one of the tools
# the prerequisites name (the curl + jq resolution fallback), so preferring it
# asks for nothing new. It is not guaranteed present -- resolution can succeed
# on gh alone, which skips the curl branch entirely -- so wget stays as a
# fallback and neither being installed is an error. Downloading with wget
# *unconditionally*, as this did before, required a tool nothing declared.
# -L is required -- release downloads redirect to objects.githubusercontent.com.
download_asset() {
    if command -v curl >/dev/null 2>&1; then
        curl -fL --retry 3 --progress-bar -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q --show-progress -O "$2" "$1"
    else
        echo "ERROR: need curl or wget to download ConformU." >&2
        echo "Install either, or download the asset by hand from:" >&2
        echo "  https://github.com/ASCOMInitiative/ConformU/releases" >&2
        return 1
    fi
}

install_conformu() {
    # Fail here rather than three steps later: the asset is a Linux x86_64 ELF,
    # so elsewhere tar and chmod both succeed and the breakage only surfaces as
    # "cannot execute binary file" at the first real run.
    local os arch
    os=$(uname -s)
    arch=$(uname -m)
    if [[ "$os" != "Linux" || "$arch" != "x86_64" ]]; then
        echo "ERROR: --install-conformu supports Linux x86_64 only (this is ${os}/${arch})." >&2
        echo "Install the matching asset by hand from:" >&2
        echo "  https://github.com/ASCOMInitiative/ConformU/releases" >&2
        return 1
    fi

    # Keep the declaration and the assignment on separate lines. `local tag=$(...)`
    # would take its status from `local`, masking a resolution failure and
    # downloading from an empty tag; split like this, set -e aborts as intended.
    local tag
    tag=$(resolve_conformu_version)
    echo "Installing ConformU ${tag}..."

    CONFORMU_DIR="$HOME/tools/conformu"
    mkdir -p "$CONFORMU_DIR"
    cd "$CONFORMU_DIR"

    rm -f "$CONFORMU_ASSET"

    echo "Downloading ConformU..."
    download_asset \
        "https://github.com/ASCOMInitiative/ConformU/releases/download/${tag}/${CONFORMU_ASSET}" \
        "$CONFORMU_ASSET"

    echo "Extracting ConformU..."
    tar -xf "$CONFORMU_ASSET"
    chmod +x conformu

    echo "ConformU installed to: $CONFORMU_DIR/conformu"
    echo "Version: $(./conformu --version 2>/dev/null || echo 'Unknown')"
}

run_conformance_tests() {
    local PORT=${1:-11111}
    local CONFIG_FILE=${2:-""}
    local TEST_DIR=${3:-"/tmp/conformu-test-$$"}
    local KEEP_REPORTS=${4:-false}
    local VERBOSE=${5:-false}
    
    echo "Running ASCOM Alpaca conformance tests..."
    echo "Port: $PORT"
    echo "Test directory: $TEST_DIR"
    echo "----------------------------------------"
    
    # Check ConformU installation
    CONFORMU_PATH="$HOME/tools/conformu/conformu"
    if [[ ! -x "$CONFORMU_PATH" ]]; then
        echo "ConformU not found at: $CONFORMU_PATH"
        echo "Run: $0 --install-conformu"
        exit 1
    fi
    
    # Build filemonitor
    echo "Building filemonitor..."
    cd "$SCRIPT_DIR"
    cargo build --release -p filemonitor
    
    # Create test environment
    mkdir -p "$TEST_DIR"
    
    # Create or use provided config
    if [[ -n "$CONFIG_FILE" && -f "$CONFIG_FILE" ]]; then
        cp "$CONFIG_FILE" "$TEST_DIR/config.json"
        echo "Using provided config: $CONFIG_FILE"
    else
        echo "Creating test configuration..."
        cat > "$TEST_DIR/config.json" << EOFCONFIG
{
  "device": {
    "name": "File Safety Monitor Test",
    "unique_id": "filemonitor-test-001",
    "description": "ASCOM Alpaca SafetyMonitor for conformance testing"
  },
  "file": {
    "path": "$TEST_DIR/RoofStatusFile.txt",
    "polling_interval_seconds": 5
  },
  "parsing": {
    "rules": [
      {
        "type": "contains",
        "pattern": "CLOSED",
        "safe": true
      },
      {
        "type": "contains", 
        "pattern": "OPEN",
        "safe": false
      },
      {
        "type": "regex",
        "pattern": "Status:\\\\s*(SAFE|OK)",
        "safe": true
      }
    ],
    "default_safe": false,
    "case_sensitive": false
  },
  "server": {
    "port": $PORT,
    "device_number": 0
  }
}
EOFCONFIG
    fi
    
    # Create test status file
    echo "2025-12-22 08:00:00 Roof Status: CLOSED" > "$TEST_DIR/RoofStatusFile.txt"
    
    # Start filemonitor service
    echo "Starting filemonitor service on port $PORT..."
    cd "$TEST_DIR"
    
    if [[ "$VERBOSE" == true ]]; then
        timeout 300 "$SCRIPT_DIR/../target/release/filemonitor" -c config.json &
    else
        timeout 300 "$SCRIPT_DIR/../target/release/filemonitor" -c config.json > "$TEST_DIR/filemonitor.log" 2>&1 &
    fi
    
    FILEMONITOR_PID=$!
    echo "Service PID: $FILEMONITOR_PID"
    
    # Wait for service to start
    echo "Waiting for service to start..."
    SERVICE_STARTED=false
    for i in {1..30}; do
        if curl -s "http://localhost:$PORT/management/v1/description" >/dev/null 2>&1; then
            echo "✅ Service started successfully"
            SERVICE_STARTED=true
            break
        fi
        if [[ $i -eq 30 ]]; then
            echo "❌ Service failed to start after 60 seconds"
            if [[ -f "$TEST_DIR/filemonitor.log" ]]; then
                echo "Service logs:"
                cat "$TEST_DIR/filemonitor.log"
            fi
            kill $FILEMONITOR_PID 2>/dev/null || true
            exit 1
        fi
        echo "Waiting... ($i/30)"
        sleep 2
    done
    
    # Test basic connectivity
    echo "Testing basic connectivity..."
    DEVICE_URL="http://localhost:$PORT/api/v1/safetymonitor/0"
    if curl -s "$DEVICE_URL/connected" | grep -q '"Value"'; then
        echo "✅ Device responds to API calls"
    else
        echo "❌ Device not responding to API calls"
        kill $FILEMONITOR_PID 2>/dev/null || true
        exit 1
    fi
    
    # Run conformance tests
    echo ""
    echo "Running ConformU conformance test..."
    CONFORMANCE_ARGS=(
        "conformance"
        "$DEVICE_URL"
        "--logfilename" "$TEST_DIR/conformance.log"
        "--resultsfile" "$TEST_DIR/conformance-report.json"
    )
    
    if [[ "$VERBOSE" == true ]]; then
        echo "Running: $CONFORMU_PATH ${CONFORMANCE_ARGS[*]}"
    fi
    
    "$CONFORMU_PATH" "${CONFORMANCE_ARGS[@]}" || CONFORMANCE_RESULT=$?
    
    echo ""
    echo "Running ConformU Alpaca protocol test..."
    PROTOCOL_ARGS=(
        "alpacaprotocol"
        "$DEVICE_URL"
        "--logfilename" "$TEST_DIR/alpaca-protocol.log"
        "--resultsfile" "$TEST_DIR/alpaca-protocol-report.json"
    )
    
    if [[ "$VERBOSE" == true ]]; then
        echo "Running: $CONFORMU_PATH ${PROTOCOL_ARGS[*]}"
    fi
    
    "$CONFORMU_PATH" "${PROTOCOL_ARGS[@]}" || PROTOCOL_RESULT=$?
    
    # Cleanup service
    echo ""
    echo "Stopping filemonitor service..."
    kill $FILEMONITOR_PID 2>/dev/null || true
    wait $FILEMONITOR_PID 2>/dev/null || true
    
    # Analyze results
    echo "Analyzing test results..."
    OVERALL_SUCCESS=true
    
    if [[ -f "$TEST_DIR/conformance-report.json" ]]; then
        if command -v jq &> /dev/null; then
            ISSUES=$(jq -r '.Issues[]? | "\(.Key): \(.Value)"' "$TEST_DIR/conformance-report.json" 2>/dev/null || echo "")
            ISSUE_COUNT=$(jq -r '.IssueCount' "$TEST_DIR/conformance-report.json" 2>/dev/null || echo "0")
            ERROR_COUNT=$(jq -r '.ErrorCount' "$TEST_DIR/conformance-report.json" 2>/dev/null || echo "0")
            
            echo "Conformance Test Results:"
            echo "  Errors: $ERROR_COUNT"
            echo "  Issues: $ISSUE_COUNT"
            
            if [[ "$ERROR_COUNT" -gt 0 ]]; then
                echo "❌ Conformance errors found - these must be fixed"
                OVERALL_SUCCESS=false
            elif [[ "$ISSUE_COUNT" -gt 0 ]]; then
                echo "⚠️  Conformance issues found (minor):"
                echo "$ISSUES"
                echo "Note: These are minor issues that don't prevent device operation"
            else
                echo "✅ All conformance tests passed"
            fi
        else
            echo "⚠️  jq not installed, cannot parse JSON results"
            echo "Check $TEST_DIR/conformance-report.json manually"
        fi
    else
        echo "❌ Conformance report not generated"
        OVERALL_SUCCESS=false
    fi
    
    if [[ -f "$TEST_DIR/alpaca-protocol-report.json" ]]; then
        if command -v jq &> /dev/null; then
            PROTOCOL_ISSUES=$(jq -r '.TestResults[] | select(.Outcome == "Issue" or .Outcome == "Error") | .TestName' "$TEST_DIR/alpaca-protocol-report.json" 2>/dev/null || echo "")
            PROTOCOL_PASSED=$(jq -r '.TestResults[] | select(.Outcome == "OK") | .TestName' "$TEST_DIR/alpaca-protocol-report.json" 2>/dev/null | wc -l || echo "0")
            PROTOCOL_TOTAL=$(jq -r '.TestResults[] | .TestName' "$TEST_DIR/alpaca-protocol-report.json" 2>/dev/null | wc -l || echo "0")
            
            echo "Protocol Test Results: $PROTOCOL_PASSED/$PROTOCOL_TOTAL tests passed"
            
            if [[ -n "$PROTOCOL_ISSUES" ]]; then
                echo "❌ Protocol issues found:"
                echo "$PROTOCOL_ISSUES"
                OVERALL_SUCCESS=false
            else
                echo "✅ All protocol tests passed"
            fi
        fi
    else
        echo "❌ Protocol report not generated"
        OVERALL_SUCCESS=false
    fi
    
    # Report locations
    echo ""
    echo "Test reports saved in: $TEST_DIR"
    echo "  - conformance.log"
    echo "  - conformance-report.json"
    echo "  - alpaca-protocol.log"
    echo "  - alpaca-protocol-report.json"
    echo "  - filemonitor.log"
    
    # Cleanup if requested
    if [[ "$KEEP_REPORTS" != true ]]; then
        echo ""
        read -p "Delete test reports? [y/N] " -n 1 -r
        echo
        if [[ $REPLY =~ ^[Yy]$ ]]; then
            rm -rf "$TEST_DIR"
            echo "Test reports deleted"
        fi
    fi
    
    if [[ "$OVERALL_SUCCESS" == true ]]; then
        echo ""
        echo "🎉 All conformance tests passed!"
        return 0
    elif [[ "$ERROR_COUNT" -eq 0 && "$ISSUE_COUNT" -gt 0 ]]; then
        echo ""
        echo "✅ Conformance tests passed with minor issues"
        echo "The device is fully functional and ASCOM compliant"
        return 0
    else
        echo ""
        echo "❌ Some conformance tests failed"
        return 1
    fi
}

# Parse command line arguments
PORT=11111
CONFIG_FILE=""
TEST_DIR=""
KEEP_REPORTS=false
VERBOSE=false
INSTALL_ONLY=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --install-conformu)
            INSTALL_ONLY=true
            shift
            ;;
        --port)
            PORT="$2"
            shift 2
            ;;
        --config)
            CONFIG_FILE="$2"
            shift 2
            ;;
        --test-dir)
            TEST_DIR="$2"
            shift 2
            ;;
        --keep-reports)
            KEEP_REPORTS=true
            shift
            ;;
        --verbose)
            VERBOSE=true
            shift
            ;;
        -h|--help)
            show_help
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            show_help
            exit 1
            ;;
    esac
done

# Set default test directory if not provided
if [[ -z "$TEST_DIR" ]]; then
    TEST_DIR="/tmp/conformu-test-$$"
fi

# Install ConformU if requested
if [[ "$INSTALL_ONLY" == true ]]; then
    install_conformu
    exit 0
fi

# Run conformance tests
run_conformance_tests "$PORT" "$CONFIG_FILE" "$TEST_DIR" "$KEEP_REPORTS" "$VERBOSE"
