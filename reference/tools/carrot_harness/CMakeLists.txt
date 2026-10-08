# Tenero's Carrot vector harness, built inside a Monero checkout by run.sh (never part of Tenero's build).
add_executable(tenero_carrot_vectors carrot_vectors.cpp)
target_link_libraries(tenero_carrot_vectors PRIVATE carrot_core ringct cncrypto epee mx25519_static ${EXTRA_LIBRARIES})
