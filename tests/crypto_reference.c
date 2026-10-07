/* Independent OpenSSL EVP reference for the Tink raw AES128_GCM_HKDF_1MB wire format.
 * Build with: cc tests/crypto_reference.c -lcrypto -o tests/crypto_reference
 */
#include <openssl/core_names.h>
#include <openssl/evp.h>
#include <openssl/kdf.h>
#include <openssl/params.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

int main(void) {
  static const uint8_t aad[] = "tink cross-language vector";
  static const uint8_t plaintext[] = "Tink-compatible reference payload";
  uint8_t key[32], salt[16], header[24], nonce[12], derived[16];
  uint8_t ciphertext[sizeof(plaintext) + 16];
  int written = 0, final_written = 0;

  for (size_t i = 0; i < sizeof(key); i++) key[i] = (uint8_t)i;
  for (size_t i = 0; i < sizeof(salt); i++) salt[i] = (uint8_t)i;
  header[0] = sizeof(header);
  memcpy(header + 1, salt, sizeof(salt));
  for (size_t i = 0; i < 7; i++) header[17 + i] = (uint8_t)(0x10 + i);
  memcpy(nonce, header + 17, 7);
  memset(nonce + 7, 0, 4);
  nonce[11] = 1;

  EVP_KDF *kdf = EVP_KDF_fetch(NULL, "HKDF", NULL);
  EVP_KDF_CTX *kdf_ctx = EVP_KDF_CTX_new(kdf);
  char digest[] = "SHA256";
  OSSL_PARAM params[] = {
      OSSL_PARAM_construct_utf8_string(OSSL_KDF_PARAM_DIGEST, digest, 0),
      OSSL_PARAM_construct_octet_string(OSSL_KDF_PARAM_KEY, key, sizeof(key)),
      OSSL_PARAM_construct_octet_string(OSSL_KDF_PARAM_SALT, salt, sizeof(salt)),
      OSSL_PARAM_construct_octet_string(OSSL_KDF_PARAM_INFO, (void *)aad, sizeof(aad) - 1),
      OSSL_PARAM_construct_end()};
  if (EVP_KDF_derive(kdf_ctx, derived, sizeof(derived), params) != 1) return 1;

  EVP_CIPHER_CTX *cipher_ctx = EVP_CIPHER_CTX_new();
  if (EVP_EncryptInit_ex(cipher_ctx, EVP_aes_128_gcm(), NULL, NULL, NULL) != 1 ||
      EVP_CIPHER_CTX_ctrl(cipher_ctx, EVP_CTRL_GCM_SET_IVLEN, sizeof(nonce), NULL) != 1 ||
      EVP_EncryptInit_ex(cipher_ctx, NULL, NULL, derived, nonce) != 1 ||
      EVP_EncryptUpdate(cipher_ctx, ciphertext, &written, plaintext,
                        sizeof(plaintext) - 1) != 1 ||
      EVP_EncryptFinal_ex(cipher_ctx, ciphertext + written, &final_written) != 1)
    return 1;
  written += final_written;
  if (EVP_CIPHER_CTX_ctrl(cipher_ctx, EVP_CTRL_GCM_GET_TAG, 16, ciphertext + written) != 1)
    return 1;
  written += 16;

  for (size_t i = 0; i < sizeof(header); i++) printf("%02x", header[i]);
  for (int i = 0; i < written; i++) printf("%02x", ciphertext[i]);
  putchar('\n');
  EVP_CIPHER_CTX_free(cipher_ctx);
  EVP_KDF_CTX_free(kdf_ctx);
  EVP_KDF_free(kdf);
  return 0;
}
