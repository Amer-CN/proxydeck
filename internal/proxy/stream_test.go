package proxy

import (
	"context"
	"strings"
	"testing"
)

func TestForEachLineNoLengthLimit(t *testing.T) {
	// A 5MB line: exceeds the old 1MB bufio.Scanner cap, must still be read intact.
	big := strings.Repeat("x", 5*1024*1024)
	input := "{\"type\":\"text-delta\",\"text\":\"start\"}\n" + big + "\n{\"type\":\"finish\",\"finishReason\":\"stop\"}\n"

	var lines []string
	err := forEachLine(strings.NewReader(input), context.Background(), func(line string) error {
		lines = append(lines, line)
		return nil
	})
	if err != nil {
		t.Fatalf("forEachLine failed: %v", err)
	}
	if len(lines) != 3 {
		t.Fatalf("expected 3 lines, got %d", len(lines))
	}
	if len(lines[1]) != len(big) {
		t.Fatalf("expected line length %d, got %d", len(big), len(lines[1]))
	}
}

func TestForEachLineCallbackErrorPropagates(t *testing.T) {
	input := "one\ntwo\n"
	err := forEachLine(strings.NewReader(input), context.Background(), func(line string) error {
		if line == "two" {
			return context.Canceled
		}
		return nil
	})
	if err != context.Canceled {
		t.Fatalf("expected callback error to propagate, got %v", err)
	}
}
