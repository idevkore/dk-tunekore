cmake_minimum_required(VERSION 3.21)
project(musikore_engine VERSION 0.1.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 20)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)
set(CMAKE_POSITION_INDEPENDENT_CODE ON)

option(MUSIKORE_BUILD_TESTS "Build the engine tests" ON)

include(FetchContent)

# RtAudio 6.0.1 (MIT-style licence), pinned by URL and SHA256.
set(BUILD_SHARED_LIBS OFF CACHE BOOL "" FORCE)
set(RTAUDIO_STATIC_MSVCRT OFF CACHE BOOL "" FORCE)  # match Rust's /MD
set(RTAUDIO_BUILD_TESTING OFF CACHE BOOL "" FORCE)
set(RTAUDIO_BUILD_STATIC_LIBS ON CACHE BOOL "" FORCE)
FetchContent_Declare(rtaudio
  URL https://github.com/thestk/rtaudio/archive/refs/tags/6.0.1.tar.gz
  URL_HASH SHA256=7206c8b6cee43b474f43d64988fefaadfdcfc4264ed38d8de5f5d0e6ddb0a123
  DOWNLOAD_EXTRACT_TIMESTAMP TRUE
  EXCLUDE_FROM_ALL)  # skip RtAudio's own install rules
FetchContent_MakeAvailable(rtaudio)

add_library(musikore_engine STATIC src/engine.cpp)
target_include_directories(musikore_engine PUBLIC include)
target_compile_definitions(musikore_engine PRIVATE
  MUSIKORE_ENGINE_VERSION="${PROJECT_VERSION}")
target_link_libraries(musikore_engine PRIVATE rtaudio)
if(MSVC)
  target_compile_options(musikore_engine PRIVATE /W4 /permissive-)
else()
  target_compile_options(musikore_engine PRIVATE -Wall -Wextra -Wpedantic)
endif()

install(TARGETS musikore_engine rtaudio ARCHIVE DESTINATION lib)
install(DIRECTORY include/ DESTINATION include)

if(MUSIKORE_BUILD_TESTS)
  enable_testing()
  add_executable(tone_test tests/tone_test.cpp)
  target_include_directories(tone_test PRIVATE src)
  add_test(NAME tone_test COMMAND tone_test)
endif()
