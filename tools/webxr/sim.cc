// Standalone simulation of Chromium's syscall-broker /proc/self handling,
// before and after the FramePlayer patch, against the real /proc of this host.
//
// The "sandboxed" parent forwards path requests to a forked "broker" child over
// a socketpair; the broker resolves them in its own process context, exactly as
// Chromium's BrokerHost does, with or without the bare-link rewrite.
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#include <optional>
#include <string>
#include <string_view>

static const char kProcSelf[] = "/proc/self/";
static constexpr std::string_view kProcSelfLink = "/proc/self";

struct Req { char op; char path[PATH_MAX]; };
struct Rep { int result; char data[PATH_MAX]; };

static std::optional<std::string> Rewrite(const char* p, pid_t pid, bool patched) {
  if (strncmp(p, kProcSelf, sizeof(kProcSelf) - 1) == 0)
    return "/proc/" + std::to_string(pid) + "/" + (p + sizeof(kProcSelf) - 1);
  if (patched && kProcSelfLink == p) return "/proc/" + std::to_string(pid);
  return std::nullopt;
}

static void BrokerLoop(int fd, pid_t sandboxed_pid, bool patched) {
  Req rq; Rep rp;
  while (read(fd, &rq, sizeof(rq)) == (ssize_t)sizeof(rq)) {
    memset(&rp, 0, sizeof(rp));
    const char* path = rq.path;
    if (rq.op == 'r' && patched && kProcSelfLink == path) {       // ReadlinkFileForIPC special case
      std::string pid = std::to_string(sandboxed_pid);
      rp.result = (int)pid.size(); memcpy(rp.data, pid.data(), pid.size());
    } else {
      auto rw = Rewrite(path, sandboxed_pid, patched);
      if (rw) path = rw->c_str();
      if (rq.op == 'r') {
        ssize_t n = readlink(path, rp.data, sizeof(rp.data) - 1);
        rp.result = n < 0 ? -errno : (int)n;
      } else if (rq.op == 's') {
        struct stat sb; rp.result = stat(path, &sb) < 0 ? -errno : 0;
        if (rp.result == 0) snprintf(rp.data, sizeof rp.data, "%lu", (unsigned long)sb.st_ino);
      } else if (rq.op == 'o') {                                    // open + read first line
        int f = open(path, O_RDONLY); rp.result = f < 0 ? -errno : 0;
        if (f >= 0) { ssize_t n = read(f, rp.data, 200); if (n > 0) rp.data[n] = 0; close(f); }
      }
    }
    if (write(fd, &rp, sizeof(rp)) != (ssize_t)sizeof(rp)) break;
  }
  _exit(0);
}

static Rep Ask(int fd, char op, const char* path) {
  Req rq{}; rq.op = op; snprintf(rq.path, sizeof rq.path, "%s", path);
  Rep rp{}; if (write(fd, &rq, sizeof rq) < 0 || read(fd, &rp, sizeof rp) < 0) perror("ipc");
  return rp;
}

// glibc-style realpath() through the broker: readlink every component.
static std::string BrokerRealpath(int fd, const char* in) {
  std::string out = "/", rest = in + 1;
  while (!rest.empty()) {
    size_t slash = rest.find('/');
    std::string comp = rest.substr(0, slash);
    rest = slash == std::string::npos ? "" : rest.substr(slash + 1);
    std::string cand = (out == "/" ? "" : out) + "/" + comp;
    Rep r = Ask(fd, 'r', cand.c_str());
    if (r.result == -EINVAL) out = cand;                              // not a symlink
    else if (r.result < 0) return "<error " + std::to_string(-r.result) + " at " + cand + ">";
    else { std::string tgt(r.data, r.result); out = tgt[0] == '/' ? tgt : (out == "/" ? "" : out) + "/" + tgt; }
  }
  return out;
}

static int Run(bool patched) {
  int sv[2]; if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) < 0) { perror("socketpair"); return 1; }
  pid_t me = getpid();
  pid_t broker = fork();
  if (broker == 0) { close(sv[0]); BrokerLoop(sv[1], me, patched); }
  close(sv[1]);
  int fails = 0;
  auto check = [&](const char* what, bool ok, const std::string& got) {
    printf("  %-44s %s  (%s)\n", what, ok ? "PASS" : "FAIL", got.c_str()); if (!ok) ++fails; };

  char buf[64]; ssize_t n = readlink("/proc/self", buf, sizeof buf - 1); buf[n > 0 ? n : 0] = 0;
  std::string kernel_self = buf;                       // what the kernel tells the sandboxed process
  Rep r = Ask(sv[0], 'r', "/proc/self");
  check("readlink(/proc/self) == own pid", r.result > 0 && std::string(r.data, r.result) == kernel_self,
        r.result > 0 ? std::string(r.data, r.result) + " vs own " + kernel_self : "errno " + std::to_string(-r.result));

  struct stat sb; stat("/proc/self", &sb);
  r = Ask(sv[0], 's', "/proc/self");
  check("stat(/proc/self) is own /proc dir", r.result == 0 && std::string(r.data) == std::to_string(sb.st_ino),
        r.result == 0 ? "ino " + std::string(r.data) + " vs own " + std::to_string(sb.st_ino) : "errno " + std::to_string(-r.result));

  r = Ask(sv[0], 'o', "/proc/self/status");
  std::string want = "Name:\tsim\n"; std::string got(r.data);
  bool pid_ok = got.find("Pid:\t" + kernel_self + "\n") != std::string::npos;
  check("open(/proc/self/status) reports own Pid:", r.result == 0 && pid_ok, r.result == 0 ? (pid_ok ? "Pid matches" : "Pid is the broker's") : "errno");

  char exe[PATH_MAX]; n = readlink("/proc/self/exe", exe, sizeof exe - 1); exe[n > 0 ? n : 0] = 0;
  std::string rp = BrokerRealpath(sv[0], "/proc/self/exe");
  check("realpath(/proc/self/exe) via broker", rp == exe, rp);

  close(sv[0]); waitpid(broker, nullptr, 0);
  return fails;
}

int main() {
  printf("sandboxed pid %d\n", getpid());
  printf("[unpatched broker: upstream behaviour]\n"); int a = Run(false);
  printf("[patched broker: bare /proc/self rewritten]\n"); int b = Run(true);
  printf("\nunpatched failures: %d, patched failures: %d\n", a, b);
  return b;
}
