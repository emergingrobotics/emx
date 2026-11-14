;;; emx-difftest.el --- run emx differential cases through GNU Emacs  -*- lexical-binding: t; -*-

;; Usage: emacs --batch -Q -l difftest/emx-difftest.el --cases FILE --out FILE
;;
;; FILE holds one JSON object per line: {"name", "text", "keys", "tier"}.
;; In "text", `|' marks point and an optional `^' marks the mark.  Each
;; case runs in a fresh fundamental-mode *scratch* buffer with Transient
;; Mark mode on, then one JSON line is written per case with the buffer
;; text, point and mark as 0-based UTF-8 byte offsets, whether the mark is
;; active, the first kills, whether the buffer is modified, and the error
;; message that ended the keyboard macro, if any.
;;
;; Each case file runs in its own Emacs process.  Emacs numbers windows
;; across the whole process and never reuses a number, so cases whose
;; messages name a window (`#<window 1 on *scratch*>') live in
;; cases/m3-window-numbers.jsonl, where no case deletes window 1 and
;; every case starts in it.

(require 'json)

(defconst emx-difftest-buffer-name "*scratch*"
  "The buffer emx's harness edits.")

(defconst emx-difftest-kill-ring-compared 3
  "How many kill-ring entries are compared.")

(defun emx-difftest--arguments ()
  "Return (CASES . OUT) from the command line and consume the arguments."
  (let ((arguments command-line-args-left)
        cases out)
    (while arguments
      (pcase (pop arguments)
        ("--cases" (setq cases (pop arguments)))
        ("--out" (setq out (pop arguments)))
        (other (error "Unknown argument: %s" other))))
    (setq command-line-args-left nil)
    (unless (and cases out)
      (error "Usage: --cases FILE --out FILE"))
    (cons cases out)))

(defun emx-difftest--read-cases (file)
  "Parse FILE as JSON lines into a list of hash tables."
  (with-temp-buffer
    (let ((coding-system-for-read 'utf-8-unix))
      (insert-file-contents file))
    (let (cases)
      (dolist (line (split-string (buffer-string) "\n" t))
        (push (json-parse-string line) cases))
      (nreverse cases))))

(defun emx-difftest--parse-text (marked)
  "Split MARKED into (CONTENT POINT MARK); positions are 0-based characters."
  (let (characters point mark (index 0))
    (dolist (character (string-to-list marked))
      (cond ((eq character ?|)
             (when point (error "Two points in %S" marked))
             (setq point index))
            ((eq character ?^)
             (when mark (error "Two marks in %S" marked))
             (setq mark index))
            (t (push character characters)
               (setq index (1+ index)))))
    (unless point (error "No point in %S" marked))
    (list (apply #'string (nreverse characters)) point mark)))

(defun emx-difftest--byte-offset (position)
  "The 0-based UTF-8 byte offset of buffer POSITION."
  (length (encode-coding-string
           (buffer-substring-no-properties (point-min) position)
           'utf-8-unix t)))

(defvar emx-difftest--initial-buffers nil
  "The buffers batch Emacs has before any case; cases kill the rest.")

(defun emx-difftest--setup ()
  "Make batch Emacs's buffers match a fresh emx session's."
  (setq emx-difftest--initial-buffers (buffer-list))
  ;; Batch Emacs has no redisplay, so `format-mode-line' returns "" and
  ;; the Buffer Menu's Mode column would be empty. A string with no
  ;; %-construct, such as a `mode-name', formats as itself.
  (advice-add 'format-mode-line :around
              (lambda (original format &rest arguments)
                (if (and (stringp format) (not (string-search "%" format)))
                    format
                  (apply original format arguments)))))

(defun emx-difftest--fresh-buffer (content point mark)
  "Make a fresh *scratch* holding CONTENT and reset the editing state.
Buffers and windows an earlier case made are removed."
  ;; Internal buffers (a leading space), such as the one the results are
  ;; written from, are not the cases' and are never listed.
  (dolist (buffer (buffer-list))
    (unless (or (memq buffer emx-difftest--initial-buffers)
                (string-prefix-p " " (buffer-name buffer)))
      (kill-buffer buffer)))
  (let ((old (get-buffer emx-difftest-buffer-name)))
    (when old (kill-buffer old)))
  (select-window (frame-first-window))
  (delete-other-windows)
  (switch-to-buffer (get-buffer-create emx-difftest-buffer-name))
  (fundamental-mode)
  ;; Killing the last ordinary buffer makes Emacs recreate *scratch* with
  ;; `initial-scratch-message'.
  (erase-buffer)
  (transient-mark-mode 1)
  (insert content)
  (set-marker (mark-marker) (and mark (1+ mark)))
  ;; A fresh emx session's *Messages* is empty and unmodified.
  (with-current-buffer (messages-buffer)
    (let ((inhibit-read-only t))
      (erase-buffer))
    (set-buffer-modified-p nil))
  (setq mark-active nil
        mark-ring nil
        global-mark-ring nil
        kill-ring nil
        kill-ring-yank-pointer nil
        last-command nil
        this-command nil
        real-last-command nil
        prefix-arg nil
        current-prefix-arg nil
        temporary-goal-column 0
        pending-undo-list nil
        undo-auto--last-boundary-cause nil
        yank-undo-function nil
        ;; A fresh emx session has searched and replaced nothing yet.
        search-ring nil
        regexp-search-ring nil
        search-ring-yank-pointer nil
        regexp-search-ring-yank-pointer nil
        isearch-case-fold-search t
        query-replace-defaults nil
        query-replace-history nil
        regexp-history nil
        occur-collect-regexp-history nil
        recenter-last-op nil)
  (goto-char (1+ point))
  (setq buffer-undo-list nil)
  (set-buffer-modified-p nil))

(defun emx-difftest--kills ()
  "The newest kills, without text properties."
  (let (kills (tail kill-ring) (count 0))
    (while (and tail (< count emx-difftest-kill-ring-compared))
      (push (substring-no-properties (car tail)) kills)
      (setq tail (cdr tail)
            count (1+ count)))
    (vconcat (nreverse kills))))

(defun emx-difftest--run (case)
  "Run CASE and return its result as an alist for `json-serialize'."
  (pcase-let* ((name (gethash "name" case))
               (keys (gethash "keys" case))
               (`(,content ,point ,mark)
                (emx-difftest--parse-text (gethash "text" case)))
               (error-message nil))
    (emx-difftest--fresh-buffer content point mark)
    ;; File commands (`find-file', `save-buffer', `write-file') must act in
    ;; a private, empty directory so they neither read nor overwrite another
    ;; case's files nor the repository, and so each side of the differential
    ;; starts from an identical empty directory. emx's harness does the same.
    (let ((case-directory (file-name-as-directory
                           (make-temp-file "emx-difftest-case-" t))))
      (setq default-directory case-directory)
      (unwind-protect
          (condition-case failure
              (execute-kbd-macro (kbd keys))
            ((error quit)
             (setq error-message (error-message-string failure))))
        (delete-directory (directory-file-name case-directory) t)))
    (let ((mark-position (mark t)))
      `((name . ,name)
        (text . ,(buffer-substring-no-properties (point-min) (point-max)))
        (point . ,(emx-difftest--byte-offset (point)))
        (mark . ,(if mark-position
                     (emx-difftest--byte-offset mark-position)
                   :null))
        (mark_active . ,(if mark-active t :false))
        (kill_ring . ,(emx-difftest--kills))
        (modified . ,(if (buffer-modified-p) t :false))
        (error . ,(or error-message :null))))))

(emx-difftest--setup)

(let* ((arguments (emx-difftest--arguments))
       (cases (emx-difftest--read-cases (car arguments)))
       (coding-system-for-write 'utf-8-unix))
  (with-temp-file (cdr arguments)
    (dolist (case cases)
      (let ((result (save-current-buffer (emx-difftest--run case))))
        (insert (json-serialize result) "\n")))))

;;; emx-difftest.el ends here
