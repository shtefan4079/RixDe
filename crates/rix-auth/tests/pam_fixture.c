#include <security/pam_modules.h>
#include <security/pam_appl.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>

static void trace(pam_handle_t *pam, const char *text) {
    const char *path = pam_getenv(pam, "RIX_TEST_TRACE");
    if (!path) return;
    FILE *out = fopen(path, "a");
    if (out) { fprintf(out, "%s\n", text); fclose(out); }
}
int pam_sm_authenticate(pam_handle_t *pam, int flags, int argc, const char **argv) {
    (void) flags; (void) argc; (void) argv;
    const struct pam_conv *conv = NULL;
    if (pam_get_item(pam, PAM_CONV, (const void **) &conv) != PAM_SUCCESS) return PAM_AUTH_ERR;
    const struct pam_message message = { PAM_PROMPT_ECHO_OFF, "Password:" };
    const struct pam_message *messages[] = { &message };
    struct pam_response *responses = NULL;
    if (conv->conv(1, messages, &responses, conv->appdata_ptr) != PAM_SUCCESS) return PAM_AUTH_ERR;
    int success = responses && responses[0].resp && strcmp(responses[0].resp, "fixture-password") == 0;
    if (responses) {
        if (responses[0].resp) { memset(responses[0].resp, 0, strlen(responses[0].resp)); free(responses[0].resp); }
        free(responses);
    }
    return success ? PAM_SUCCESS : PAM_AUTH_ERR;
}
int pam_sm_acct_mgmt(pam_handle_t *pam, int flags, int argc, const char **argv) {
    (void) flags; (void) argc; (void) argv;
    const char *user = NULL;
    if (pam_get_user(pam, &user, NULL) != PAM_SUCCESS) return PAM_USER_UNKNOWN;
    return strcmp(user, "fixture-account") == 0 ? PAM_SUCCESS : PAM_USER_UNKNOWN;
}
int pam_sm_setcred(pam_handle_t *pam, int flags, int argc, const char **argv) {
    (void) argc; (void) argv;
    trace(pam, flags & PAM_DELETE_CRED ? "credentials-deleted" : "credentials-established");
    return PAM_SUCCESS;
}
int pam_sm_open_session(pam_handle_t *pam, int flags, int argc, const char **argv) {
    (void) flags; (void) argc; (void) argv;
    trace(pam, "session-open");
    if (pam_getenv(pam, "RIX_TEST_FAIL_OPEN_BEFORE_ID")) return PAM_SESSION_ERR;
    int result = pam_putenv(pam, "XDG_SESSION_ID=fixture");
    if (result != PAM_SUCCESS) return result;
    return pam_getenv(pam, "RIX_TEST_FAIL_OPEN") ? PAM_SESSION_ERR : PAM_SUCCESS;
}
int pam_sm_close_session(pam_handle_t *pam, int flags, int argc, const char **argv) {
    (void) flags; (void) argc; (void) argv;
    trace(pam, "session-close");
    return PAM_SUCCESS;
}
