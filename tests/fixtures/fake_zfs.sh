#!/usr/bin/env bash
# Fake `zfs` for tests/zfs_backend. tests/zfs_backend/support.rs fills in the
# mode (scenario name), base (fixture directory) and pid_file placeholders
# below before installing it as SNAPSHOT_TO_S3_ZFS_BIN.
# Every invocation is appended to $base/commands; $base/<name>.json files
# override the generated JSON, and receive records its PID in the PID file.
set -euo pipefail
mode="__MODE__"
pid_file="__PID_FILE__"
base="__BASE__"
cmd="$1"
shift || true
printf '%s %s\n' "$cmd" "$*" >> "$base/commands"

reject_args() {
  echo "unexpected arguments for $cmd: $*" >&2
  exit 99
}

override() {
  if [[ -f "$base/$1.json" ]]; then
    cat "$base/$1.json"
    exit 0
  fi
}

get_json() {
  printf '{"output_version":{"command":"zfs get","vers_major":0,"vers_minor":1},"datasets":{"%s":{"name":"%s","type":"%s","pool":"pool","createtxg":"1","properties":{%s}}}}\n' "$last" "$last" "$kind" "$1"
}

case "$cmd" in
  get)
    [[ "$#" == 4 && "$1" == "-j" && "$2" == "-p" ]] || reject_args "$@"
    property="$3"
    last="$4"
    kind="FILESYSTEM"
    [[ "$last" != *"@"* ]] || kind="SNAPSHOT"
    [[ "$last" != "pool/vol" ]] || kind="VOLUME"
    if [[ "$property" == "type" ]]; then
      override get-type
      if [[ "$mode" == "huge-stdout" ]]; then
        i=0
        while [[ $i -lt 9000 ]]; do
          printf '0123456789'
          i=$((i+1))
        done
        printf '\n'
        exit 0
      fi
      if [[ "$mode" == "type-op-error" ]]; then
        echo "permission denied" >&2
        exit 1
      fi
      if [[ "$last" == "pool/fs" || "$last" == "pool/new" ]]; then
        get_json '"type":{"value":"filesystem","source":{"type":"NONE","data":"-"}}'
      elif [[ "$last" == "pool/vol" ]]; then
        get_json '"type":{"value":"volume","source":{"type":"NONE","data":"-"}}'
      else
        echo "dataset does not exist" >&2
        exit 1
      fi
      exit 0
    fi
    if [[ "$property" == "guid" ]]; then
      override get-guid
      if [[ "$last" == "pool/fs" || "$last" == "pool/new" ]]; then
        get_json '"guid":{"value":"22","source":{"type":"NONE","data":"-"}}'
        exit 0
      fi
      echo "dataset does not exist" >&2
      exit 1
    fi

    if [[ "$property" == "guid,createtxg" ]]; then
      override get-snapshot
      if [[ "$last" == "pool/fs@s1" || "$last" == "pool/fs@s2" ]]; then
        get_json '"guid":{"value":"11","source":{"type":"NONE","data":"-"}},"createtxg":{"value":"101","source":{"type":"NONE","data":"-"}}'
        exit 0
      fi
      if [[ "$last" == "pool/fs/child@sx" ]]; then
        get_json '"guid":{"value":"33","source":{"type":"NONE","data":"-"}},"createtxg":{"value":"99","source":{"type":"NONE","data":"-"}}'
        exit 0
      fi
      echo "snapshot does not exist" >&2
      exit 1
    fi

    if [[ "$property" == "written@s1" ]]; then
      override get-written
      case "$mode" in
        candidate-invalid)
          echo "not an earlier snapshot from the same fs" >&2
          exit 1
          ;;
        written-op-error)
          echo "permission denied" >&2
          exit 1
          ;;
        *)
          get_json '"written@s1":{"value":"4096","source":{"type":"NONE","data":"-"}}'
          ;;
      esac
      exit 0
    fi
    reject_args "$@"
    ;;

  list)
    [[ "$#" == 11 && "$1" == "-j" && "$2" == "-p" && "$3" == "-t" && "$4" == "snapshot" && "$5" == "-o" && "$6" == "name" && "$7" == "-d" && "$8" == "1" && "$9" == "-s" && "${10}" == "creation" && "${11}" == "pool/fs" ]] || reject_args "$@"
    override list
    if [[ "$mode" == "list-child-leak" ]]; then
      second="pool/fs/child@sx"
    else
      second="pool/fs@s2"
    fi
    printf '{"output_version":{"command":"zfs list","vers_major":0,"vers_minor":1},"datasets":{"pool/fs@s1":{"name":"pool/fs@s1","type":"SNAPSHOT","pool":"pool","createtxg":"1","properties":{}},"%s":{"name":"%s","type":"SNAPSHOT","pool":"pool","createtxg":"1","properties":{}}}}\n' "$second" "$second"
    ;;

  send)
    for arg in "$@"; do
      [[ "$arg" != "-j" && "$arg" != "--json" ]] || reject_args "$@"
    done
    joined=" $* "
    if [[ "$joined" == *" -nP "* ]]; then
      [[ "$*" == "-nP -w pool/fs@s2" || "$*" == "-nP -w -i pool/fs@s1 pool/fs@s2" ]] || reject_args "$@"
      if [[ "$mode" == "candidate-invalid" ]]; then
        echo "incremental source invalid" >&2
        exit 1
      fi
      printf 'size\t8192\n'
      exit 0
    fi
    [[ "$*" == "-w pool/fs@s2" || "$*" == "-w -i pool/fs@s1 pool/fs@s2" ]] || reject_args "$@"

    case "$mode" in
      send-fail-stderr)
        i=0
        while [[ $i -lt 8000 ]]; do
          echo "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789" >&2
          i=$((i+1))
        done
        exit 2
        ;;
      send-stream)
        i=0
        while [[ $i -lt 500 ]]; do
          printf 'chunk-%d\n' "$i"
          i=$((i+1))
          sleep 0.01
        done
        ;;
      *)
        printf 'stream-data\n'
        ;;
    esac
    ;;

  diff)
    [[ "$*" == "-H pool/fs@s2 pool/fs" ]] || reject_args "$@"
    if [[ "$mode" == "dirty" ]]; then
      printf 'M\tpool/fs/file\n'
    fi
    ;;

  receive)
    [[ "$*" == "-u pool/new" ]] || reject_args "$@"
    case "$mode" in
      receive-exit-fail)
        cat >/dev/null
        echo "cannot receive" >&2
        exit 3
        ;;
      receive-hang-pid)
        printf '%s\n' "$$" > "$pid_file"
        cat >/dev/null
        ;;
      receive-hang)
        cat >/dev/null
        ;;
      *)
        cat >/dev/null
        ;;
    esac
    ;;

  *)
    echo "unsupported command: $cmd" >&2
    exit 99
    ;;
esac
