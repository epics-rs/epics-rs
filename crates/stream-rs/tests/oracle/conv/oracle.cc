// The standard converters of StreamFormatConverter.cc, called the way
// StreamCore calls them, for tests/std_converters.rs (see run-c.sh).
//
// Each stdin line is mode<TAB>format<TAB>arg[<TAB>size]. The format starts
// at its '%' (or the ')' of a field name). Mode p* parses it as an output
// format, s* as an input format, c* as an input format that '=' turns into
// an output one; the second letter is l, d or s for long, double or string.
// String and input args are hex bytes, "." for none; size is scanString's
// buffer size (16 when left out, 0 for a NULL buffer). Mode pp runs
// printPseudo on an output that holds arg, sp scanPseudo on input arg with
// the cursor at the fourth field (0 when left out); both with streamDebug
// on, so their debug lines land in the error bytes. A \xNN in the
// format is that byte. STREAM_DEBUG sets streamDebug. Each stdout line
// is what parseFormat returned, the format text left after it, the info
// string, the call's result and what StreamError printed, bytes in hex.
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <vector>
#include "StreamFormatConverter.h"
#include "StreamProtocol.h"
#include "StreamError.h"

static std::string hexdec(const char* h) {
    std::string s; if (h[0] == 0x2e) return s; while (h[0] && h[1]) { unsigned v; sscanf(h, "%2x", &v); s.push_back((char)v); h += 2; } return s;
}
static void hexout(const char* p, size_t n) { for (size_t i = 0; i < n; i++) printf("%02x", (unsigned char)p[i]); }

int main() {
    streamError = 1;
    if (getenv("STREAM_DEBUG")) streamDebug = atoi(getenv("STREAM_DEBUG"));
    streamMsgTimeStamped = 0; streamDebugColored = 0;
    extern FILE* StreamDebugFile;
    char line[8192];
    setvbuf(stdout, NULL, _IONBF, 0);
    while (fgets(line, sizeof line, stdin)) {
        line[strcspn(line, "\n")] = 0;
        std::vector<char*> f; char* save; for (char* t = strtok_r(line, "\t", &save); t; t = strtok_r(NULL, "\t", &save)) f.push_back(t);
        if (f.size() < 3) continue;
        std::string mode = f[0];
        bool scan = mode[0] == 's' || mode[0] == 'c';
        std::string fs;
        for (const char* p = f[1]; *p; p++) {
            unsigned v;
            if (p[0] == '\\' && p[1] == 'x' && sscanf(p + 2, "%2x", &v) == 1) { fs.push_back((char)v); p += 3; }
            else fs.push_back(*p);
        }
        const char* src = fs.c_str();
        StreamFormat fmt; StreamBuffer info;
        memset(&fmt, 0, sizeof fmt);
        char* eb = NULL; size_t el = 0; StreamDebugFile = open_memstream(&eb, &el);
        int t = StreamFormatConverter::parseFormat(src, scan ? ScanFormat : PrintFormat, fmt, info);
        printf("parse=%d rest=", t); hexout(src, strlen(src));
        printf(" infolen=%zu info=", info.length()); hexout(info(), info.length());
        if (!t) { fclose(StreamDebugFile); StreamDebugFile = NULL; printf(" err="); hexout(eb, el); free(eb); printf("\n"); continue; }
        fmt.type = (StreamFormatType)t; fmt.info = info(); fmt.infolen = info.length();
        StreamFormatConverter* c = StreamFormatConverter::find(fmt.conv);
        StreamBuffer out; bool ok = false; ssize_t n = 0;
        if (mode == "pl" || mode == "cl") { ok = c->printLong(fmt, out, strtol(f[2], NULL, 0)); printf(" ok=%d out=", ok); hexout(out(), out.length()); }
        else if (mode == "pd" || mode == "cd") { ok = c->printDouble(fmt, out, strtod(f[2], NULL)); printf(" ok=%d out=", ok); hexout(out(), out.length()); }
        else if (mode == "ps" || mode == "cs") { std::string v = hexdec(f[2]); ok = c->printString(fmt, out, v.c_str()); printf(" ok=%d out=", ok); hexout(out(), out.length()); }
        else if (mode == "sl") { std::string v = hexdec(f[2]); long val = 0x5a5a; n = c->scanLong(fmt, v.c_str(), val); printf(" n=%zd val=%ld", n, val); }
        else if (mode == "sd") { std::string v = hexdec(f[2]); double val = 1234.5; n = c->scanDouble(fmt, v.c_str(), val); unsigned long bits; memcpy(&bits, &val, 8); printf(" n=%zd val=%016lx", n, bits); }
        else if (mode == "pp") { std::string v = hexdec(f[2]); StreamBuffer o(v.data(), v.size()); streamDebug = 1; ok = c->printPseudo(fmt, o); streamDebug = 0; printf(" ok=%d out=", ok); hexout(o(), o.length()); }
        else if (mode == "sp") { std::string v = hexdec(f[2]); StreamBuffer in(v.data(), v.size()); size_t cursor = f.size() > 3 ? strtoul(f[3], NULL, 0) : 0;
            streamDebug = 1; n = c->scanPseudo(fmt, in, cursor); streamDebug = 0; printf(" n=%zd cursor=%zu", n, cursor); }
        else if (mode == "ss") { std::string v = hexdec(f[2]); size_t size = f.size() > 3 ? strtoul(f[3], NULL, 0) : 16; char buf[256]; memset(buf, 0x5a, sizeof buf);
            size_t sz = size; n = c->scanString(fmt, v.c_str(), size ? buf : NULL, sz); printf(" n=%zd size=%zu buf=", n, sz); hexout(buf, size < 256 ? size : 256); }
        fclose(StreamDebugFile); StreamDebugFile = NULL;
        printf(" err="); hexout(eb, el); free(eb);
        printf("\n");
    }
}
