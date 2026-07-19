// Command rag-ingestion-orchestrator is the network/orchestration layer of
// the pipeline. It deliberately does NOT touch PDF internals -- that is the
// Rust core's job -- and instead focuses on what Go is good at: accepting
// large concurrent uploads without ballooning its own memory, and streaming
// results back to the client incrementally.
//
// Deliberate trade-off: FFI (cgo bindings into a Rust cdylib) vs. subprocess
// pipes. The spec's architecture diagram shows "Zero-Copy Memory Stream
// Transfer" via FFI/WASM. We use a subprocess + stdin/stdout pipe instead:
//   - No cgo toolchain coupling between the Go and Rust build pipelines
//     (cgo requires matching ABI/allocator assumptions and complicates
//     cross-compilation).
//   - Process-level isolation: a panic or OOM in the Rust core can't take
//     the Go server down with it, which matters more for a 500-page
//     worst-case document than the (small, one-time) cost of a process
//     spawn and a pipe.
//   - It's a straightforward drop-in to swap for a cdylib + cgo binding, or
//     for gRPC to a separate Rust worker pool, later, without changing the
//     HTTP-facing contract. See README for that upgrade path.
package main

import (
	"bufio"
	"fmt"
	"io"
	"log"
	"mime"
	"mime/multipart"
	"net/http"
	"os"
	"os/exec"
	"strings"
	"sync"
	"time"
)

const maxUploadBytes = 2 << 30 // 2GB worst-case guard; tune per deployment

func rustCorePath() string {
	if p := os.Getenv("RUST_CORE_PATH"); p != "" {
		return p
	}
	return "../rust-core/target/release/rag_ingestion_core"
}

func main() {
	mux := http.NewServeMux()
	mux.HandleFunc("/health", handleHealth)
	mux.HandleFunc("/ingest", handleIngest)

	addr := os.Getenv("LISTEN_ADDR")
	if addr == "" {
		addr = ":8080"
	}
	srv := &http.Server{
		Addr:              addr,
		Handler:           mux,
		ReadHeaderTimeout: 15 * time.Second,
		// No response-body write timeout: large-document ingestion is
		// intentionally long-running and streamed.
	}
	log.Printf("rag-ingestion-orchestrator listening on %s (rust core: %s)", addr, rustCorePath())
	log.Fatal(srv.ListenAndServe())
}

func handleHealth(w http.ResponseWriter, r *http.Request) {
	if _, err := os.Stat(rustCorePath()); err != nil {
		http.Error(w, fmt.Sprintf(`{"status":"degraded","reason":"rust core not found at %s"}`, rustCorePath()), http.StatusServiceUnavailable)
		return
	}
	w.Header().Set("Content-Type", "application/json")
	fmt.Fprint(w, `{"status":"ok"}`)
}

// handleIngest streams the uploaded PDF straight to disk via a true
// multipart.Reader (never buffering the whole request body in Go's heap
// the way http.Request.ParseMultipartForm's default memory threshold can),
// then spawns the Rust core against that file and streams its NDJSON
// output back to the client as it's produced -- so a caller ingesting a
// 500-page document starts seeing page-fidelity reports and chunks well
// before the last page has even been parsed.
func handleIngest(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodPost {
		http.Error(w, "POST required", http.StatusMethodNotAllowed)
		return
	}
	r.Body = http.MaxBytesReader(w, r.Body, maxUploadBytes)

	_, params, err := mime.ParseMediaType(r.Header.Get("Content-Type"))
	if err != nil || !strings.HasPrefix(r.Header.Get("Content-Type"), "multipart/") {
		http.Error(w, "expected multipart/form-data with a 'file' field", http.StatusBadRequest)
		return
	}
	boundary, ok := params["boundary"]
	if !ok {
		http.Error(w, "missing multipart boundary", http.StatusBadRequest)
		return
	}

	mr := multipart.NewReader(r.Body, boundary)
	documentID := "document"
	tmpFile, err := os.CreateTemp("", "ingest-*.pdf")
	if err != nil {
		http.Error(w, "failed to allocate temp storage", http.StatusInternalServerError)
		return
	}
	tmpPath := tmpFile.Name()
	defer os.Remove(tmpPath)

	var fileReceived bool
	for {
		part, err := mr.NextPart()
		if err == io.EOF {
			break
		}
		if err != nil {
			tmpFile.Close()
			http.Error(w, "malformed multipart body", http.StatusBadRequest)
			return
		}
		switch part.FormName() {
		case "file":
			// Streams directly from the request body to disk in bounded
			// chunks (io.Copy's default 32KB buffer) rather than reading
			// the whole part into a []byte first.
			if _, err := io.Copy(tmpFile, part); err != nil {
				tmpFile.Close()
				http.Error(w, "failed while streaming upload to disk", http.StatusInternalServerError)
				return
			}
			fileReceived = true
		case "document_id":
			buf := make([]byte, 256)
			n, _ := part.Read(buf)
			if n > 0 {
				documentID = strings.TrimSpace(string(buf[:n]))
			}
		}
		part.Close()
	}
	tmpFile.Close()

	if !fileReceived {
		http.Error(w, "no 'file' part found in multipart body", http.StatusBadRequest)
		return
	}

	streamRustCore(w, tmpPath, documentID)
}

// streamRustCore spawns the Rust processing core against the given file
// path and relays both its NDJSON stdout (page_fidelity / chunk / summary
// records) and its stderr log lines (pass1/pass2 stats, quarantine ALERTs)
// to the HTTP client as they're produced, flushing after every line.
func streamRustCore(w http.ResponseWriter, filePath, documentID string) {
	flusher, canFlush := w.(http.Flusher)

	cmd := exec.Command(rustCorePath(), filePath, documentID)
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		http.Error(w, "failed to open core stdout", http.StatusInternalServerError)
		return
	}
	stderr, err := cmd.StderrPipe()
	if err != nil {
		http.Error(w, "failed to open core stderr", http.StatusInternalServerError)
		return
	}

	w.Header().Set("Content-Type", "application/x-ndjson")
	w.WriteHeader(http.StatusOK)

	if err := cmd.Start(); err != nil {
		fmt.Fprintf(w, `{"type":"fatal_error","message":%q}`+"\n", err.Error())
		return
	}

	done := make(chan struct{})
	var writeMu sync.Mutex
	safeWrite := func(format string, args ...interface{}) {
		writeMu.Lock()
		defer writeMu.Unlock()
		fmt.Fprintf(w, format, args...)
		if canFlush {
			flusher.Flush()
		}
	}

	go func() {
		defer close(done)
		scanner := bufio.NewScanner(stderr)
		scanner.Buffer(make([]byte, 0, 64*1024), 1024*1024)
		for scanner.Scan() {
			safeWrite("{\"type\":\"log\",\"message\":%q}\n", scanner.Text())
		}
	}()

	scanner := bufio.NewScanner(stdout)
	scanner.Buffer(make([]byte, 0, 256*1024), 8*1024*1024) // pages with big tables can produce long lines
	for scanner.Scan() {
		safeWrite("%s\n", scanner.Text())
	}

	<-done
	if err := cmd.Wait(); err != nil {
		safeWrite("{\"type\":\"fatal_error\",\"message\":%q}\n", err.Error())
	}
}
